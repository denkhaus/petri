//! Workflow → HIR, per spec §12.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use frontend::FileSource;
use frontend::diag::{Diagnostic, Diagnostics, Lowered, Span};
use frontend::expr::lower::builtin;
use frontend::expr::parse;
use frontend::yaml::{Document, Node};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    BinOp, Budget, ExpandTarget, ExprId, ExprOrValue, GraphBuilder, NodeId, RuntimeSpec, Scope,
    ScopeId, StepRef, UnOp, ValidationError, Value,
};
use serde_json::{Map, json};
use smol_str::SmolStr;

use crate::action::{
    ACTION_KIND, ActionLocation, ActionRef, ActionSource, ActionSourceError, Phase, PinnedAction,
    RUN_KIND, STATE_OUTPUT_KEY,
};
use crate::composite::{self, NodeAction, Runs, Uses};
use crate::exprs::{LoweredScalar, SEP, Site, config_value, lower_scalar, secret_sentinel};
use crate::model::{Defaults, Job, KNOWN_RUNS_ON, Step, Workflow};

/// GitHub's default job timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(360 * 60);

struct JobNodes {
    scope: ScopeId,
    start: NodeId,
    done: NodeId,
    /// The last node of the step chain, which the expansion region ends at.
    last: NodeId,
    matrix: bool,
}

pub struct Lowering<'w, 'a> {
    b: GraphBuilder,
    diags: Diagnostics,
    wf: &'w Workflow<'a>,
    files: &'w dyn FileSource,
    /// Where `uses: owner/repo@ref` actions come from. `None` rejects them.
    actions: Option<&'w dyn ActionSource>,
    /// Remote actions resolved so far, by reference as written: the pin and the
    /// manifest text, or why not. A reference used by several steps resolves once.
    resolved: HashMap<String, Result<(PinnedAction, String), ResolveFailure>>,
    jobs: HashMap<String, JobNodes>,
    spans: HashMap<NodeId, Span>,
}

/// Why a remote action did not resolve.
#[derive(Clone)]
enum ResolveFailure {
    /// No [`ActionSource`] was given: the format is running without one.
    NoSource,
    /// The source said [`ActionSourceError::Unavailable`]: it does not serve this
    /// reference. Rejected like `NoSource`, scoped to the one reference.
    Unavailable,
    Failed(String),
}

/// A `uses:` step that is a JavaScript action, resolved once for the nodes it
/// contributes: `pre` and `post` are placed away from the main one.
struct ActionPlan {
    location: ActionLocation,
    node: NodeAction,
    inputs: Vec<PlanInput>,
}

struct PlanInput {
    name: String,
    /// The default's text, expressions and all; lowered where the step is.
    default: Option<String>,
    required: bool,
}

pub fn lower(
    wf: &Workflow<'_>,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    diags: Diagnostics,
) -> Lowered {
    let mut lw = Lowering {
        b: GraphBuilder::bare(),
        diags,
        wf,
        files,
        actions,
        resolved: HashMap::new(),
        jobs: HashMap::new(),
        spans: HashMap::new(),
    };

    // Every job's scope and gate first, so `needs` can wire to them in any order.
    for job in &wf.jobs {
        lw.job_shell(job);
    }
    for job in &wf.jobs {
        lw.job_body(job);
    }
    for job in &wf.jobs {
        lw.job_edges(job);
    }

    if lw.diags.has_errors() {
        return Lowered::rejected(lw.diags);
    }
    let builder = std::mem::replace(&mut lw.b, GraphBuilder::bare());
    let mut graph = builder.build();
    ir::normalize_loop_heads(&mut graph);
    let report = ir::check(&graph);
    for error in &report.errors {
        let span = lw.span_for(error);
        lw.diags.error(
            &format!("validate.{}", variant(error)),
            span,
            error.to_string(),
        );
    }
    for warning in &report.warnings {
        let span = lw.spans.get(&warning.at()).cloned().unwrap_or_default();
        let mut d = Diagnostic::warning(warning.code(), span, warning.to_string());
        if let Some(hint) = warning.hint() {
            d = d.with_hint(hint);
        }
        lw.diags.push(d);
    }
    Lowered::from_parts(graph, lw.diags)
}

impl<'w, 'a> Lowering<'w, 'a> {
    // ── Jobs ───────────────────────────────────────────────────────────────

    /// The scope, `start` and `done` nodes for a job.
    /// The job's site, as its own `env:`, `if:` and `outputs:` see it: `needs` known,
    /// matrix-ness known, no steps yet.
    fn base_site(&self, job: &Job<'a>) -> Site {
        let mut site = Site::new(&job.id);
        site.matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        for (need, _) in &job.needs {
            site.needs.insert(need.clone(), format!("{need}{SEP}done"));
        }
        site
    }

    fn job_shell(&mut self, job: &Job<'a>) {
        let matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        let scope = self.scope_for(job);
        let start = self.b.add_node(
            &format!("{}{SEP}start", job.id),
            scope,
            StepRef::new("noop", json!({ "job": job.id, "phase": "start" })),
        );
        self.spans.insert(start, job.span.clone());
        let done = self.b.add_node(
            &format!("{}{SEP}done", job.id),
            scope,
            StepRef::new("noop", Value::Null),
        );
        // Every `done` runs on cancel, unconditionally: it is a side-effect-free
        // fold, and a dependent's `always()` gate and `needs.J.*` reads need a
        // truthful summary in every cancel case.
        self.b.node_mut(done).run_on_cancel = true;
        self.spans.insert(done, job.span.clone());
        self.jobs.insert(
            job.id.clone(),
            JobNodes {
                scope,
                start,
                done,
                last: start,
                matrix,
            },
        );
    }

    fn scope_for(&mut self, job: &Job<'a>) -> ScopeId {
        let mut scope = Scope::new(ScopeId::new(0));
        scope.runtime = self.runtime_for(job);

        // Default environment GitHub gives every step, from the run parameters.
        let site = self.base_site(job);
        let github = |this: &mut Self, key: &str| -> ExprOrValue {
            let t = this.b.exprs();
            let ctx = t.var("github");
            let k = t.lit(key);
            let v = t.call("get_ci", vec![ctx, k]);
            let s = t.call("loose_string", vec![v]);
            ExprOrValue::Expr(s)
        };
        scope
            .env
            .insert("CI".into(), ExprOrValue::Value(json!("true")));
        scope
            .env
            .insert("GITHUB_ACTIONS".into(), ExprOrValue::Value(json!("true")));
        scope
            .env
            .insert("GITHUB_JOB".into(), ExprOrValue::Value(json!(job.id)));
        if let Some(name) = &self.wf.name {
            scope
                .env
                .insert("GITHUB_WORKFLOW".into(), ExprOrValue::Value(json!(name)));
        }
        for key in [
            "sha",
            "ref",
            "ref_name",
            "ref_type",
            "repository",
            "repository_owner",
            "actor",
            "event_name",
            "run_id",
            "run_number",
            "run_attempt",
            "server_url",
            "api_url",
            "base_ref",
            "head_ref",
        ] {
            let var = format!("GITHUB_{}", key.to_uppercase());
            scope.env.insert(SmolStr::new(&var), github(self, key));
        }
        for key in ["os", "arch", "name", "temp", "tool_cache"] {
            let t = self.b.exprs();
            let ctx = t.var("runner");
            let k = t.lit(key);
            let v = t.call("get_ci", vec![ctx, k]);
            let s = t.call("loose_string", vec![v]);
            scope.env.insert(
                SmolStr::new(format!("RUNNER_{}", key.to_uppercase())),
                ExprOrValue::Expr(s),
            );
        }

        // Workflow env, then job env on top. Secret refs cannot live in scope env
        // (there is nowhere for them to go but the process), so they are pushed down
        // into every step's env instead; `job_body` reads them back from `site`.
        for (key, node) in self.wf.env.iter().chain(job.env.iter()) {
            match self.env_value(node, &site, false) {
                Some(EnvValue::Plain(v)) => {
                    scope.env.insert(SmolStr::new(key), v);
                }
                Some(EnvValue::Secret(_)) => {
                    // Recorded in job_body via the same lookup; nothing to do here.
                }
                None => {}
            }
        }
        self.b.add_scope(scope)
    }

    fn runtime_for(&mut self, job: &Job<'a>) -> RuntimeSpec {
        let mut spec = RuntimeSpec::host_process();

        match job.runs_on {
            None if job.reusable => {}
            None => {
                self.diags.error(
                    "gha.no_runs_on",
                    job.span.clone(),
                    format!("job `{}` has no `runs-on`", job.id),
                );
            }
            Some(node) => {
                let labels: Vec<(String, Span)> = if let Some(s) = node.as_str() {
                    vec![(s.to_string(), node.span())]
                } else if let Some(seq) = node.as_sequence() {
                    seq.iter()
                        .filter_map(|n| n.as_str().map(|s| (s.to_string(), n.span())))
                        .collect()
                } else if let Some(m) = node.as_mapping() {
                    // `runs-on: { group: …, labels: … }`
                    let mut out = Vec::new();
                    if let Some(labels) = m.get("labels") {
                        if let Some(one) = labels.as_str() {
                            out.push((one.to_string(), labels.span()));
                        } else if let Some(seq) = labels.as_sequence() {
                            out.extend(
                                seq.iter()
                                    .filter_map(|n| n.as_str().map(|s| (s.to_string(), n.span()))),
                            );
                        }
                    }
                    if let Some(group) = m.get("group") {
                        self.diags.unsupported(
                            "runs_on.group",
                            group.span(),
                            "`runs-on.group` names a runner group",
                            "runner groups are a GitHub-hosted concept; name a label instead",
                        );
                    }
                    out
                } else {
                    Vec::new()
                };
                for (label, span) in labels {
                    if label.contains("${{") {
                        self.diags.unsupported(
                            "runs_on.expression",
                            span,
                            format!("`runs-on: {label}` is an expression"),
                            "each matrix leg would need its own environment, and the IR has one scope per job \
                             (a spec finding); use a fixed label, or one job per OS",
                        );
                        continue;
                    }
                    let lowered = label.to_lowercase();
                    if lowered.starts_with("windows") {
                        self.diags.unsupported(
                            "runs_on.windows",
                            span,
                            format!("`runs-on: {label}`"),
                            "Windows is v2",
                        );
                    } else if lowered == "self-hosted" || !KNOWN_RUNS_ON.contains(&lowered.as_str())
                    {
                        self.diags.unsupported(
                            "runs_on.unknown",
                            span,
                            format!("`runs-on: {label}` is not a label the local executor knows"),
                            &format!("known labels: {}", KNOWN_RUNS_ON.join(", ")),
                        );
                    }
                    spec.requirements.push(SmolStr::new(label));
                }
            }
        }

        if let Some(container) = job.container {
            let image = if let Some(s) = container.as_str() {
                Some((s.to_string(), container.span()))
            } else if let Some(m) = container.as_mapping() {
                for (key, span) in m.keys() {
                    match key {
                        "image" | "env" => {}
                        other => self.diags.unsupported(
                            &format!("container.{other}"),
                            span,
                            format!("`container.{other}`"),
                            "only `image` and `env` are mapped onto a container scope; `options` are engine flags, which the graph does not carry",
                        ),
                    }
                }
                m.get("image")
                    .and_then(|i| i.as_str().map(|s| (s.to_string(), i.span())))
            } else {
                None
            };
            match image {
                Some((image, span)) if image.contains("${{") => {
                    self.diags.unsupported(
                        "container.expression",
                        span,
                        "an expression-valued container image",
                        "the image is fixed per scope; use a literal",
                    );
                }
                Some((image, _)) => {
                    let requirements = std::mem::take(&mut spec.requirements);
                    spec = RuntimeSpec::container(&image);
                    spec.requirements = requirements;
                }
                None => self.diags.error(
                    "gha.bad_container",
                    container.span(),
                    "`container` needs an `image`",
                ),
            }
        }
        spec
    }

    /// Steps, preconditions, config, and the matrix expansion.
    fn job_body(&mut self, job: &Job<'a>) {
        let shell = self
            .jobs
            .get(&job.id)
            .map(|j| (j.scope, j.start, j.done, j.matrix));
        let Some((scope, start, done, matrix)) = shell else {
            return;
        };

        // Secrets at workflow/job level are pushed down into every step.
        let mut site = self.base_site(job);
        let mut job_secret_env: Vec<(String, String)> = Vec::new();
        for (key, node) in self.wf.env.iter().chain(job.env.iter()) {
            // Lowered once already for the scope; here only to find the secrets, whose
            // diagnostics (if any) were reported then.
            let mut scratch = Diagnostics::new();
            let saved = std::mem::replace(&mut self.diags, scratch);
            let value = self.env_value(node, &site, false);
            scratch = std::mem::replace(&mut self.diags, saved);
            let _ = scratch;
            if let Some(EnvValue::Secret(name)) = value {
                job_secret_env.push((key.clone(), name));
            }
        }
        let _ = matrix;

        // Matrix.
        let mut matrix_items: Option<ExprId> = None;
        if let Some(strategy) = job.strategy.as_ref().filter(|s| s.matrix.is_some()) {
            let matrix_node = strategy.matrix.expect("filtered");
            let matrix_expr = self.matrix_expr(matrix_node, &site);
            site.matrix_total = self.static_matrix_total(matrix_node);
            site.fail_fast = match strategy
                .fail_fast
                .and_then(|n| n.as_scalar())
                .map(|s| (s.as_bool(), s.as_str().to_string()))
            {
                None => true,
                Some((Some(b), _)) => b,
                Some((None, text)) => {
                    self.diags.unsupported(
                        "strategy.fail_fast.expression",
                        strategy.fail_fast.map(|n| n.span()).unwrap_or_default(),
                        format!("`fail-fast: {text}`"),
                        "use a literal true or false",
                    );
                    true
                }
            };
            site.max_parallel = match strategy
                .max_parallel
                .and_then(|n| n.as_scalar())
                .map(|s| (s.as_i64(), s.as_str().to_string()))
            {
                None => None,
                Some((Some(n), _)) if n >= 1 => Some(n as u32),
                Some((_, text)) => {
                    self.diags.unsupported(
                        "strategy.max_parallel.expression",
                        strategy.max_parallel.map(|n| n.span()).unwrap_or_default(),
                        format!("`max-parallel: {text}`"),
                        "use a literal integer",
                    );
                    None
                }
            };
            if let Some(m) = matrix_expr {
                matrix_items = crate::expr_lower::matrix_legs(self.b.exprs(), m).ok();
            }
        }

        // The gate: needs + the job's own if.
        let gate = self.condition(job.condition, &site, false, job.span.clone());
        if let Some(gate) = gate {
            self.b.set_precondition(start, gate);
        }

        // Steps.
        let mut names_so_far: Vec<String> = Vec::new();
        let mut step_names: BTreeMap<String, String> = BTreeMap::new();
        for step in &job.steps {
            step_names.insert(
                step.node_name(),
                format!("{}{SEP}{}", job.id, step.node_name()),
            );
        }
        site.step_names = step_names;

        // A JavaScript action with `pre` or `post` contributes nodes away from its
        // own position: GitHub runs every `pre` before the first step and every
        // `post` after the last, in reverse order. Resolved once, quietly; the main
        // pass reports whatever is wrong with the reference.
        let plans: Vec<Option<ActionPlan>> = job
            .steps
            .iter()
            .map(|step| self.action_plan(step))
            .collect();

        let mut previous = start;
        let mut chain: Vec<NodeId> = Vec::new();
        for (step, plan) in job.steps.iter().zip(&plans) {
            if let Some(plan) = plan
                && plan.node.pre.is_some()
                && let Some(id) =
                    self.lifecycle_node(job, step, plan, Phase::Pre, scope, &site, &job_secret_env)
            {
                self.chain_node(&mut previous, &mut chain, &mut names_so_far, &mut site, id);
            }
        }
        for step in &job.steps {
            let inherited = Defaults {
                shell: step.shell.or(job.defaults.shell).or(self.wf.defaults.shell),
                working_directory: step
                    .working_directory
                    .or(job.defaults.working_directory)
                    .or(self.wf.defaults.working_directory),
            };
            let nodes = self.step_nodes(
                job,
                step,
                scope,
                &mut site,
                &names_so_far,
                &job_secret_env,
                inherited,
                0,
            );
            for id in nodes {
                self.chain_node(&mut previous, &mut chain, &mut names_so_far, &mut site, id);
            }
        }
        for (step, plan) in job.steps.iter().zip(&plans).rev() {
            if let Some(plan) = plan
                && plan.node.post.is_some()
                && let Some(id) =
                    self.lifecycle_node(job, step, plan, Phase::Post, scope, &site, &job_secret_env)
            {
                self.chain_node(&mut previous, &mut chain, &mut names_so_far, &mut site, id);
            }
        }

        // A job whose own `if:` names `always()` or `cancelled()` is a cleanup job:
        // GitHub admits the whole job after a cancel, so every node of it — start,
        // steps, done (flagged already) — must be able to fire. The interior steps'
        // own gates then keep their normal meaning, observed over this job's steps.
        // Matrix legs are covered too: clones inherit the flag from these templates.
        if names_cleanup(job.condition) {
            self.b.node_mut(start).run_on_cancel = true;
            for id in &chain {
                self.b.node_mut(*id).run_on_cancel = true;
            }
        }

        let last = *chain.last().unwrap_or(&start);
        if let Some(j) = self.jobs.get_mut(&job.id) {
            j.last = last;
        }

        // The last step's edge carries the job summary into `done`. It is evaluated in
        // the last step's outcome context, so `status` is that step's own.
        let summary = self.job_summary(job, &site, last == start);
        let ids = self
            .b
            .select(last, vec![ir::Arm::always(done).with_map(summary)]);
        let _ = ids;

        // `done` folds its inputs — one for a plain job, one per leg for a matrix.
        let fold = self.done_config();
        self.b.node_mut(done).step = StepRef::new("noop", fold);

        if let Some(items) = matrix_items {
            let target = if last == start {
                ExpandTarget::Node
            } else {
                ExpandTarget::Subgraph {
                    entry: start,
                    exit: last,
                }
            };
            ir::parallel_for_each(
                &mut self.b,
                start,
                items,
                target,
                site.max_parallel,
                site.fail_fast,
            );
        }
    }

    /// `done` → each dependent's `start`.
    fn job_edges(&mut self, job: &Job<'a>) {
        let Some(done) = self.jobs.get(&job.id).map(|j| j.done) else {
            return;
        };
        let dependents: Vec<NodeId> = self
            .wf
            .jobs
            .iter()
            .filter(|other| other.needs.iter().any(|(n, _)| n == &job.id))
            .filter_map(|other| self.jobs.get(&other.id).map(|j| j.start))
            .collect();
        for (need, span) in &job.needs {
            if !self.jobs.contains_key(need) {
                self.diags.error(
                    "gha.needs_unknown",
                    span.clone(),
                    format!("job `{}` needs unknown job `{need}`", job.id),
                );
            }
        }
        if !dependents.is_empty() {
            self.b.fan_out(done, &dependents);
        }
    }

    // ── Steps ──────────────────────────────────────────────────────────────

    /// Link `id` after `previous` and make it visible to the steps after it.
    fn chain_node(
        &mut self,
        previous: &mut NodeId,
        chain: &mut Vec<NodeId>,
        names: &mut Vec<String>,
        site: &mut Site,
        id: NodeId,
    ) {
        self.b.link(*previous, id);
        chain.push(id);
        *previous = id;
        let name = self
            .b
            .graph()
            .node(id)
            .map(|n| n.name.to_string())
            .unwrap_or_default();
        names.push(name);
        site.earlier_steps = names.clone();
    }

    /// The node(s) for one step: one `github/run` node, one `github/action` node,
    /// or a composite's inlined chain.
    #[allow(clippy::too_many_arguments)]
    fn step_nodes(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        scope: ScopeId,
        site: &mut Site,
        earlier: &[String],
        job_secret_env: &[(String, String)],
        inherited: Defaults<'_>,
        depth: usize,
    ) -> Vec<NodeId> {
        let node_name = match site.action_inputs.is_some() {
            true => format!("{}{SEP}{}", site.job_id, step.node_name()),
            false => format!("{}{SEP}{}", job.id, step.node_name()),
        };

        if let Some((reference, span)) = &step.uses {
            return self.uses_step(
                job,
                step,
                reference,
                span,
                scope,
                site,
                earlier,
                job_secret_env,
                depth,
            );
        }

        let mut step_site = site.clone();
        step_site.earlier_steps = earlier.to_vec();
        let env_config = self.step_env_config(step, &mut step_site, job_secret_env);

        let mut config = Map::new();
        let Some(run) = step.run else {
            return Vec::new();
        };
        let Some(run_text) = run.as_str() else {
            self.diags
                .error("gha.bad_step", run.span(), "`run` must be a string");
            return Vec::new();
        };
        match lower_scalar(
            run_text,
            run.span(),
            &step_site,
            true,
            true,
            self.b.exprs(),
            &mut self.diags,
        ) {
            Some(v) => {
                config.insert("run".into(), config_value(v));
            }
            None => return Vec::new(),
        }

        // Shell. `bash` and `sh` invoke directly; `python` and `pwsh` get GitHub's
        // documented template; any custom template with a `{0}` is taken as
        // written — the step writes the script to a file and substitutes its path,
        // as GitHub does. Windows-only shells stay rejected.
        let shell = match inherited.shell {
            None => "bash",
            Some(node) => node.as_str().unwrap_or(""),
        };
        match shell {
            "bash" => {
                config.insert("shell".into(), json!("bash"));
            }
            "sh" => {
                config.insert("shell".into(), json!("sh"));
            }
            "python" => {
                config.insert("shell_command".into(), json!("python {0}"));
            }
            "pwsh" => {
                config.insert("shell_command".into(), json!("pwsh -command \". '{0}'\""));
            }
            other if other.contains("{0}") && !other.contains("${{") => {
                config.insert("shell_command".into(), json!(other));
            }
            other => {
                let kind = other.split_whitespace().next().unwrap_or(other);
                self.diags.unsupported(
                    &format!(
                        "shell.{}",
                        kind.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                    ),
                    inherited.shell.map(|n| n.span()).unwrap_or_default(),
                    format!("`shell: {other}`"),
                    "`bash`, `sh`, `python`, `pwsh` and custom `{0}` templates are available; \
                     Windows shells are not",
                );
            }
        }

        if let Some(wd) = inherited.working_directory
            && let Some(text) = wd.as_str()
            && let Some(v) = lower_scalar(
                text,
                wd.span(),
                &step_site,
                true,
                false,
                self.b.exprs(),
                &mut self.diags,
            )
        {
            config.insert("working_dir".into(), config_value(v));
        }

        // continue-on-error → soft_fail.
        if let Some(coe) = step.continue_on_error {
            match coe.as_scalar().and_then(|s| s.as_bool()) {
                Some(true) => {
                    config.insert("soft_fail".into(), json!(true));
                }
                Some(false) => {}
                None => self.diags.unsupported(
                    "continue_on_error.expression",
                    coe.span(),
                    "an expression-valued `continue-on-error`",
                    "use a literal true or false",
                ),
            }
        }
        if !env_config.is_empty() {
            config.insert("env".into(), Value::Object(env_config));
        }
        config.insert("event".into(), self.event_config());

        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(RUN_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        self.gate_main_node(id, step, &step_site);
        vec![id]
    }

    /// The step's `env:` as config, secrets from the job pushed down first. Step env
    /// is also made visible to the step's own expressions through `site`.
    fn step_env_config(
        &mut self,
        step: &Step<'_>,
        step_site: &mut Site,
        job_secret_env: &[(String, String)],
    ) -> Map<String, Value> {
        let mut env_config = Map::new();
        for (key, name) in job_secret_env {
            env_config.insert(
                key.clone(),
                json!({ ir::placeholder::SECRET_REF_KEY: name }),
            );
        }
        for (key, node) in &step.env {
            match self.env_value(node, step_site, true) {
                Some(EnvValue::Plain(v)) => {
                    step_site.step_env.insert(key.clone(), v.clone());
                    env_config.insert(
                        key.clone(),
                        match v {
                            ExprOrValue::Value(v) => v,
                            ExprOrValue::Expr(id) => json!({ EXPR_PLACEHOLDER_KEY: id.raw() }),
                        },
                    );
                }
                Some(EnvValue::Secret(name)) => {
                    env_config.insert(
                        key.clone(),
                        json!({ ir::placeholder::SECRET_REF_KEY: name }),
                    );
                }
                None => {}
            }
        }
        env_config
    }

    /// Timeout: step, else job, else GitHub's default.
    fn set_step_budget(&mut self, id: NodeId, job: &Job<'a>, step: &Step<'_>) {
        let timeout = self
            .minutes(step.timeout_minutes)
            .or_else(|| self.minutes(job.timeout_minutes))
            .unwrap_or(DEFAULT_TIMEOUT);
        self.b.set_budget(id, Budget::new(1, timeout));
    }

    /// A main step's gate: the job started, and the step's own condition (default
    /// `success()` over earlier steps). A step gated on `always()` or `cancelled()`
    /// runs after a cancel in GitHub, so it opts in; its precondition then decides
    /// as usual.
    fn gate_main_node(&mut self, id: NodeId, step: &Step<'_>, step_site: &Site) {
        let cond = self.condition(step.condition, step_site, true, step.span.clone());
        let started = step_site.job_started(self.b.exprs());
        let pre = match cond {
            Some(c) => self.b.exprs().binary(BinOp::And, started, c),
            None => started,
        };
        self.b.set_precondition(id, pre);
        if names_cleanup(step.condition) {
            self.b.node_mut(id).run_on_cancel = true;
        }
    }

    /// `github.event`, for `GITHUB_EVENT_PATH`.
    fn event_config(&mut self) -> Value {
        let t = self.b.exprs();
        let github = t.var("github");
        let key = t.lit("event");
        let event = t.call("get_ci", vec![github, key]);
        json!({ EXPR_PLACEHOLDER_KEY: event.raw() })
    }

    /// The state an earlier phase of the same action saved, read from its record.
    fn state_config(&mut self, site: &Site, from: &str) -> Value {
        let t = self.b.exprs();
        let record = site.node_record(t, from);
        let output = t.field(record, "output");
        let key = t.lit(STATE_OUTPUT_KEY);
        let state = t.index(output, key);
        let empty = t.object(vec![]);
        let state = t.call("default", vec![state, empty]);
        json!({ EXPR_PLACEHOLDER_KEY: state.raw() })
    }

    /// `true` when the action's main node has a record other than `skipped`: the
    /// condition for its `post` to run at all.
    fn main_ran(&mut self, site: &Site, main: &str) -> ExprId {
        let t = self.b.exprs();
        let status = site.node_status(t, main);
        let skipped = t.lit("skipped");
        let status = t.call("default", vec![status, skipped]);
        let is_skipped = t.binary(BinOp::Eq, status, skipped);
        t.unary(UnOp::Not, is_skipped)
    }

    // ── `uses:` ────────────────────────────────────────────────────────────

    /// Resolve `owner/repo@ref` to its commit and manifest, once per reference.
    fn resolve_remote(&mut self, name: &str) -> Result<(PinnedAction, String), ResolveFailure> {
        if let Some(cached) = self.resolved.get(name) {
            return cached.clone();
        }
        let source_failure = |e: ActionSourceError| match e {
            ActionSourceError::Unavailable(_) => ResolveFailure::Unavailable,
            other => ResolveFailure::Failed(other.to_string()),
        };
        let result = match self.actions {
            None => Err(ResolveFailure::NoSource),
            Some(source) => ActionRef::parse(name)
                .map_err(|e| ResolveFailure::Failed(e.to_string()))
                .and_then(|reference| source.resolve(&reference).map_err(source_failure))
                .and_then(|pinned| {
                    source
                        .manifest(&pinned)
                        .map(|text| (pinned, text))
                        .map_err(source_failure)
                }),
        };
        self.resolved.insert(name.to_string(), result.clone());
        result
    }

    /// The `action.yml` document for a `uses:` reference, or the diagnostic saying
    /// why there is none.
    fn action_document(&mut self, reference: &str, span: &Span) -> Option<Document> {
        match composite::classify(reference) {
            Uses::Local(path) => composite::read_document(self.files, &path, span, &mut self.diags),
            Uses::Docker(image) => {
                self.diags.unsupported(
                    "action.docker",
                    span.clone(),
                    format!("`uses: docker://{image}`"),
                    "Docker actions run against the daemon, not through the job environment; not yet built",
                );
                None
            }
            Uses::Remote(name) => match self.resolve_remote(&name) {
                Ok((pinned, text)) => Document::parse(
                    &format!("{}/action.yml", pinned.reference),
                    &text,
                    &mut self.diags,
                ),
                Err(ResolveFailure::NoSource) => {
                    self.diags.unsupported(
                        "action.remote",
                        span.clone(),
                        name.to_string(),
                        "no action source is configured, so actions from other repositories cannot be fetched",
                    );
                    None
                }
                Err(ResolveFailure::Unavailable) => {
                    self.diags.unsupported(
                        "action.remote",
                        span.clone(),
                        name.to_string(),
                        "the configured action source does not serve this reference, so it cannot be fetched from here",
                    );
                    None
                }
                Err(ResolveFailure::Failed(message)) => {
                    self.diags.error(
                        "action.unresolved",
                        span.clone(),
                        format!("`uses: {name}`: {message}"),
                    );
                    None
                }
            },
        }
    }

    /// The plan for a `uses:` step that is a JavaScript action; `None` for anything
    /// else, quietly — the main pass reports problems.
    fn action_plan(&mut self, step: &Step<'_>) -> Option<ActionPlan> {
        let (reference, span) = step.uses.as_ref()?;
        let saved = std::mem::replace(&mut self.diags, Diagnostics::new());
        let plan = self.action_plan_inner(reference, span);
        self.diags = saved;
        plan
    }

    fn action_plan_inner(&mut self, reference: &str, span: &Span) -> Option<ActionPlan> {
        let location = self.location_of(reference)?;
        let doc = self.action_document(reference, span)?;
        let manifest = composite::read_manifest(&doc, &mut self.diags)?;
        let Runs::Node(node) = manifest.runs else {
            return None;
        };
        Some(ActionPlan {
            location,
            node,
            inputs: plan_inputs(&manifest.inputs),
        })
    }

    /// Where a `uses:` reference's files are found at run time; `None` when it
    /// names no fetched or local tree.
    fn location_of(&mut self, reference: &str) -> Option<ActionLocation> {
        match composite::classify(reference) {
            Uses::Local(path) => Some(ActionLocation::Local { local: path }),
            Uses::Docker(_) => None,
            Uses::Remote(name) => Some(ActionLocation::Pinned(self.resolve_remote(&name).ok()?.0)),
        }
    }

    /// A `pre` or `post` node for a JavaScript action. `pre-if` and `post-if`
    /// default to `always()`; a `post` also needs its main node to have run.
    #[allow(clippy::too_many_arguments)]
    fn lifecycle_node(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        plan: &ActionPlan,
        phase: Phase,
        scope: ScopeId,
        site: &Site,
        job_secret_env: &[(String, String)],
    ) -> Option<NodeId> {
        let (_, span) = step.uses.as_ref()?;
        let source = match phase {
            Phase::Pre => plan.node.pre_if.as_deref(),
            Phase::Post => plan.node.post_if.as_deref(),
            Phase::Main => return None,
        }
        .unwrap_or("always()");
        let main_name = format!("{}{SEP}{}", site.job_id, step.node_name());
        let state_from = (phase == Phase::Post).then(|| main_name.clone());
        let id = self.action_node(
            job,
            step,
            plan,
            phase,
            scope,
            site,
            job_secret_env,
            state_from.as_deref(),
        );
        let cond = self.condition_text(source, site, true, span.clone());
        let started = site.job_started(self.b.exprs());
        let mut pre = match cond {
            Some(c) => self.b.exprs().binary(BinOp::And, started, c),
            None => started,
        };
        if phase == Phase::Post {
            let ran = self.main_ran(site, &main_name);
            pre = self.b.exprs().binary(BinOp::And, pre, ran);
            // A post step is cleanup: GitHub runs it after a cancel unless its
            // `post-if` says otherwise.
            if if_calls_any(source, &["always", "cancelled"]) {
                self.b.node_mut(id).run_on_cancel = true;
            }
        }
        self.b.set_precondition(id, pre);
        Some(id)
    }

    /// One `github/action` node: the action pinned, its phase and entry point, its
    /// inputs and env lowered where the step is.
    #[allow(clippy::too_many_arguments)]
    fn action_node(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        plan: &ActionPlan,
        phase: Phase,
        scope: ScopeId,
        site: &Site,
        job_secret_env: &[(String, String)],
        state_from: Option<&str>,
    ) -> NodeId {
        let (uses, span) = step
            .uses
            .as_ref()
            .map(|(u, s)| (u.as_str(), s.clone()))
            .unwrap_or_default();
        let entry = match phase {
            Phase::Pre => plan.node.pre.clone(),
            Phase::Main => Some(plan.node.main.clone()),
            Phase::Post => plan.node.post.clone(),
        }
        .unwrap_or_default();
        let suffix = match phase {
            Phase::Pre => format!("{SEP}pre"),
            Phase::Main => String::new(),
            Phase::Post => format!("{SEP}post"),
        };
        let node_name = format!("{}{SEP}{}{suffix}", site.job_id, step.node_name());

        let mut step_site = site.clone();
        let env_config = self.step_env_config(step, &mut step_site, job_secret_env);

        // Inputs: declared ones take the caller's `with:`, else their default;
        // undeclared `with:` keys pass through as GitHub does (with a warning there).
        let mut with: BTreeMap<String, Node<'_>> = BTreeMap::new();
        for (k, v) in &step.with {
            with.insert(k.to_lowercase(), *v);
        }
        let mut inputs = Map::new();
        let mut declared: HashSet<String> = HashSet::new();
        for input in &plan.inputs {
            let key = input.name.to_lowercase();
            declared.insert(key.clone());
            let value = match with.get(&key) {
                Some(node) => self.with_value(*node, &step_site),
                None => match &input.default {
                    Some(text) => self.text_value(text, span.clone(), &step_site),
                    None => {
                        if input.required && phase == Phase::Main {
                            self.diags.error(
                                "gha.missing_input",
                                span.clone(),
                                format!("`{uses}` requires input `{}`", input.name),
                            );
                        }
                        None
                    }
                },
            };
            if let Some(value) = value {
                inputs.insert(input.name.clone(), value);
            }
        }
        for (key, node) in &step.with {
            if !declared.contains(&key.to_lowercase())
                && let Some(value) = self.with_value(*node, &step_site)
            {
                inputs.insert(key.clone(), value);
            }
        }

        let mut config = Map::new();
        config.insert(
            "action".into(),
            serde_json::to_value(&plan.location).expect("an action location serializes"),
        );
        config.insert("phase".into(), json!(phase.as_str()));
        config.insert("entry".into(), json!(entry));
        config.insert("runtime".into(), json!(plan.node.runtime));
        config.insert("inputs".into(), Value::Object(inputs));
        if !env_config.is_empty() {
            config.insert("env".into(), Value::Object(env_config));
        }
        config.insert("event".into(), self.event_config());
        if let Some(from) = state_from {
            config.insert("state".into(), self.state_config(site, from));
        }
        if let Some(coe) = step.continue_on_error {
            match coe.as_scalar().and_then(|s| s.as_bool()) {
                Some(true) => {
                    config.insert("soft_fail".into(), json!(true));
                }
                Some(false) => {}
                None if phase == Phase::Main => self.diags.unsupported(
                    "continue_on_error.expression",
                    coe.span(),
                    "an expression-valued `continue-on-error`",
                    "use a literal true or false",
                ),
                None => {}
            }
        }

        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(ACTION_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        id
    }

    /// A `with:` value: a string, possibly templated; anything else stringified.
    fn with_value(&mut self, node: Node<'_>, site: &Site) -> Option<Value> {
        match node.as_str() {
            Some(text) => self.text_value(text, node.span(), site),
            None => Some(Value::String(scalar_text(node))),
        }
    }

    /// Text from the workflow or an action manifest as an input value: literal,
    /// expression, or a whole-value secret.
    fn text_value(&mut self, text: &str, span: Span, site: &Site) -> Option<Value> {
        lower_scalar(
            text,
            span,
            site,
            true,
            true,
            self.b.exprs(),
            &mut self.diags,
        )
        .map(config_value)
    }

    /// `uses:` — a composite (local or remote) is inlined; a JavaScript action
    /// becomes a `github/action` node; a Docker action is rejected.
    #[allow(clippy::too_many_arguments)]
    fn uses_step(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        reference: &str,
        span: &Span,
        scope: ScopeId,
        site: &mut Site,
        earlier: &[String],
        job_secret_env: &[(String, String)],
        depth: usize,
    ) -> Vec<NodeId> {
        if depth >= composite::MAX_DEPTH {
            self.diags.error(
                "gha.composite_depth",
                span.clone(),
                format!(
                    "composite actions nest more than {} deep at `{reference}`",
                    composite::MAX_DEPTH
                ),
            );
            return Vec::new();
        }
        let Some(doc) = self.action_document(reference, span) else {
            return Vec::new();
        };
        let Some(manifest) = composite::read_manifest(&doc, &mut self.diags) else {
            return Vec::new();
        };
        let action = match manifest.runs {
            Runs::Composite(action) => action,
            Runs::Node(node) => {
                // The manifest in hand is the plan; no need to read it again.
                let Some(location) = self.location_of(reference) else {
                    return Vec::new();
                };
                let plan = ActionPlan {
                    location,
                    node,
                    inputs: plan_inputs(&manifest.inputs),
                };
                if depth > 0 && (plan.node.pre.is_some() || plan.node.post.is_some()) {
                    self.diags.warning(
                        "action.nested_lifecycle",
                        span.clone(),
                        format!(
                            "`{reference}` has `pre` or `post` steps, which do not run inside a composite action here; its main step does"
                        ),
                    );
                }
                let mut step_site = site.clone();
                step_site.earlier_steps = earlier.to_vec();
                let main_name = format!("{}{SEP}{}", site.job_id, step.node_name());
                let state_from = plan
                    .node
                    .pre
                    .is_some()
                    .then(|| format!("{main_name}{SEP}pre"));
                let id = self.action_node(
                    job,
                    step,
                    &plan,
                    Phase::Main,
                    scope,
                    &step_site,
                    job_secret_env,
                    state_from.as_deref(),
                );
                self.gate_main_node(id, step, &step_site);
                return vec![id];
            }
            Runs::Docker => {
                self.diags.unsupported(
                    "action.docker",
                    span.clone(),
                    format!("`{reference}` is a Docker container action"),
                    "Docker actions run against the daemon, not through the job environment; not yet built",
                );
                return Vec::new();
            }
        };
        if matches!(composite::classify(reference), Uses::Remote(_))
            && action
                .steps
                .iter()
                .any(|s| matches!(&s.uses, Some((u, _)) if u.starts_with('.')))
        {
            self.diags.unsupported(
                "action.nested_local",
                span.clone(),
                format!("`{reference}` uses a `./` action inside a remote composite"),
                "a relative action inside a fetched composite resolves against that repository, which the lowering does not stage",
            );
            return Vec::new();
        }

        // Inputs: the caller's `with`, lowered in the caller's site, else the default.
        let caller_site = {
            let mut s = site.clone();
            s.earlier_steps = earlier.to_vec();
            s
        };
        let mut inputs: BTreeMap<String, ExprId> = BTreeMap::new();
        let mut with: BTreeMap<String, Node<'_>> = BTreeMap::new();
        for (k, v) in &step.with {
            with.insert(k.to_lowercase(), *v);
        }
        for input in &action.inputs {
            let node = with
                .get(&input.name.to_lowercase())
                .copied()
                .or(input.default);
            match node {
                Some(n) => {
                    let text = n.as_str().unwrap_or("");
                    match lower_scalar(
                        text,
                        n.span(),
                        &caller_site,
                        true,
                        true,
                        self.b.exprs(),
                        &mut self.diags,
                    ) {
                        Some(LoweredScalar::Literal(v)) => {
                            let id = self.b.exprs().lit(v);
                            inputs.insert(input.name.clone(), id);
                        }
                        Some(LoweredScalar::Expr(id)) => {
                            inputs.insert(input.name.clone(), id);
                        }
                        Some(LoweredScalar::Secret(name)) => {
                            // Into the inner steps' expressions it goes as its
                            // sentinel; the step that receives it resolves it at spawn.
                            let id = self.b.exprs().lit(secret_sentinel(&name));
                            inputs.insert(input.name.clone(), id);
                        }
                        None => {}
                    }
                }
                None if input.required => {
                    self.diags.error(
                        "gha.missing_input",
                        span.clone(),
                        format!("`{reference}` requires input `{}`", input.name),
                    );
                }
                None => {
                    let id = self.b.exprs().lit(Value::Null);
                    inputs.insert(input.name.clone(), id);
                }
            }
        }

        // The composite's steps live under `job/caller/inner`.
        let caller = step.node_name();
        let mut inner_site = site.clone();
        inner_site.job_id = format!("{}{SEP}{caller}", site.job_id);
        inner_site.action_inputs = Some(inputs);
        inner_site.step_names = action
            .steps
            .iter()
            .map(|s| {
                (
                    s.node_name(),
                    format!("{}{SEP}{}", inner_site.job_id, s.node_name()),
                )
            })
            .collect();
        inner_site.composite_outputs = BTreeMap::new();

        // Inner steps observe the composite's own steps, not the caller's: the
        // action is a unit, and its steps' implicit `success()` must not re-judge
        // work outside it. The caller's gate below is what speaks for that work.
        let mut names_so_far: Vec<String> = Vec::new();
        inner_site.earlier_steps = Vec::new();
        let mut ids = Vec::new();
        for inner in &action.steps {
            let inherited = Defaults {
                shell: inner.shell,
                working_directory: inner.working_directory,
            };
            let nodes = self.step_nodes(
                job,
                inner,
                scope,
                &mut inner_site,
                &names_so_far,
                job_secret_env,
                inherited,
                depth + 1,
            );
            for id in nodes {
                let name = self
                    .b
                    .graph()
                    .node(id)
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                names_so_far.push(name);
                inner_site.earlier_steps = names_so_far.clone();
                ids.push(id);
            }
        }

        // The caller's `if:` gates the whole inlined unit the way a job's gate
        // does: there is no wrapper node, so it is ANDed into every inlined node's
        // precondition (default: `success()` over the caller's earlier steps). And
        // when it names `always()` or `cancelled()`, the composite is cleanup:
        // every inlined node opts in, mirroring the cleanup-job rule.
        let caller_gate = self.condition(step.condition, &caller_site, true, span.clone());
        let cleanup = names_cleanup(step.condition);
        for id in &ids {
            if let Some(gate) = caller_gate {
                let pre = match self.b.graph().node(*id).and_then(|n| n.precondition) {
                    Some(own) => self.b.exprs().binary(BinOp::And, gate, own),
                    None => gate,
                };
                self.b.set_precondition(*id, pre);
            }
            if cleanup {
                self.b.node_mut(*id).run_on_cancel = true;
            }
        }

        // Outputs: expressions over the inner steps, visible to the caller as
        // `steps.<caller>.outputs.<name>`.
        let mut outputs: BTreeMap<String, ExprId> = BTreeMap::new();
        for (name, value) in &action.outputs {
            let text = value.as_str().unwrap_or("");
            if let Some(lowered) = lower_scalar(
                text,
                value.span(),
                &inner_site,
                true,
                false,
                self.b.exprs(),
                &mut self.diags,
            ) {
                let id = match lowered {
                    LoweredScalar::Literal(v) => self.b.exprs().lit(v),
                    LoweredScalar::Expr(id) => id,
                    LoweredScalar::Secret(_) => continue,
                };
                outputs.insert(name.clone(), id);
            }
        }
        site.composite_outputs.insert(caller, outputs);
        ids
    }

    // ── Pieces ─────────────────────────────────────────────────────────────

    /// The `{ result, outputs }` summary the last step's edge carries into `done`,
    /// evaluated in that step's outcome context.
    fn job_summary(&mut self, job: &Job<'a>, site: &Site, no_steps: bool) -> ExprId {
        let t = self.b.exprs();
        // Own status counts too: `status` here is the last step's.
        let own = t.var("status");
        let own_failed = {
            let f = t.lit("failure");
            let to = t.lit("timed_out");
            let a = t.binary(BinOp::Eq, own, f);
            let b = t.binary(BinOp::Eq, own, to);
            t.binary(BinOp::Or, a, b)
        };
        let own_cancelled = {
            let c = t.lit("cancelled");
            t.binary(BinOp::Eq, own, c)
        };
        let earlier_failed = site.earlier_step_failed(t);
        let earlier_cancelled = site.earlier_step_cancelled(t);
        let failed = if no_steps {
            earlier_failed
        } else {
            t.binary(BinOp::Or, earlier_failed, own_failed)
        };
        let cancelled = if no_steps {
            earlier_cancelled
        } else {
            t.binary(BinOp::Or, earlier_cancelled, own_cancelled)
        };
        let started = site.job_started(t);

        let s_failure = t.lit("failure");
        let s_cancelled = t.lit("cancelled");
        let s_success = t.lit("success");
        let s_skipped = t.lit("skipped");
        let inner = t.cond(cancelled, s_cancelled, s_success);
        let ran = t.cond(failed, s_failure, inner);
        let result = t.cond(started, ran, s_skipped);

        // Job outputs, lowered where every step is visible.
        let mut outputs: Vec<(String, ExprId)> = Vec::new();
        for (name, node) in &job.outputs {
            let text = node.as_str().unwrap_or("");
            if let Some(lowered) = lower_scalar(
                text,
                node.span(),
                site,
                true,
                false,
                self.b.exprs(),
                &mut self.diags,
            ) {
                let id = match lowered {
                    LoweredScalar::Literal(v) => self.b.exprs().lit(v),
                    LoweredScalar::Expr(id) => id,
                    LoweredScalar::Secret(_) => {
                        self.diags.error(
                            "secrets.misplaced",
                            node.span(),
                            "a job output cannot be a secret",
                        );
                        continue;
                    }
                };
                outputs.push((name.clone(), id));
            }
        }
        let t = self.b.exprs();
        let outputs_obj = t.object(outputs.iter().map(|(k, v)| (k.as_str(), *v)).collect());
        let index = if site.matrix {
            t.var("index")
        } else {
            t.lit(0)
        };
        t.object(vec![
            ("result", result),
            ("outputs", outputs_obj),
            ("index", index),
        ])
    }

    /// `done`'s config: fold `inputs` (one summary per leg) into one.
    fn done_config(&mut self) -> Value {
        let t = self.b.exprs();
        let inputs = t.var("inputs");
        let result_key = t.lit("result");
        let results = t.call("pluck", vec![inputs, result_key]);
        let has = |t: &mut ir::ExprTable, tag: &str| {
            let l = t.lit(tag);
            t.call("contains", vec![results, l])
        };
        let any_failure = has(t, "failure");
        let any_cancelled = has(t, "cancelled");
        let any_success = has(t, "success");
        let f = t.lit("failure");
        let c = t.lit("cancelled");
        let s = t.lit("success");
        let k = t.lit("skipped");
        let inner2 = t.cond(any_success, s, k);
        let inner1 = t.cond(any_cancelled, c, inner2);
        let result = t.cond(any_failure, f, inner1);

        // Outputs from the last leg to report, by index.
        let index_key = t.lit("index");
        let sorted = t.call("sort_by_key", vec![inputs, index_key]);
        let outputs_key = t.lit("outputs");
        let all_outputs = t.call("pluck", vec![sorted, outputs_key]);
        let len = t.call("len", vec![all_outputs]);
        let one = t.lit(1);
        let last = t.binary(BinOp::Sub, len, one);
        let outputs = t.index(all_outputs, last);
        let empty = t.object(vec![]);
        let outputs = t.call("default", vec![outputs, empty]);

        let summary = t.object(vec![("result", result), ("outputs", outputs)]);
        json!({ EXPR_PLACEHOLDER_KEY: summary.raw() })
    }

    /// An `if:`: absent means `success()`; present is evaluated with GitHub's
    /// truthiness, and gets `success() &&` in front unless it names a status function.
    fn condition(
        &mut self,
        node: Option<Node<'_>>,
        site: &Site,
        at_step: bool,
        span: Span,
    ) -> Option<ExprId> {
        let Some(node) = node else {
            return Some(site.status_function(self.b.exprs(), "success", at_step));
        };
        let Some(scalar) = node.as_scalar() else {
            self.diags.error(
                "gha.bad_if",
                node.span(),
                "`if` must be a string or boolean",
            );
            return None;
        };
        if let Some(b) = scalar.as_bool() {
            let lit = self.b.exprs().lit(b);
            let success = site.status_function(self.b.exprs(), "success", at_step);
            return Some(self.b.exprs().binary(BinOp::And, success, lit));
        }
        let source = match if_expr_source(scalar.as_str()) {
            Ok(source) => source,
            Err(IfTemplateError::Mixed) => {
                self.diags.error(
                    "expr.mixed_condition",
                    node.span(),
                    "an `if` is one expression, bare or in a single `${{ }}`",
                );
                return None;
            }
            Err(IfTemplateError::Unterminated) => {
                self.diags
                    .error("expr.unterminated", node.span(), "unterminated `${{`");
                return None;
            }
        };
        self.condition_text(&source, site, at_step, span)
    }

    /// One condition's expression text (the body of an `if:`, a `pre-if`, a
    /// `post-if`) as a precondition: GitHub's truthiness, with `success() &&` in
    /// front unless it names a status function.
    fn condition_text(
        &mut self,
        source: &str,
        site: &Site,
        at_step: bool,
        span: Span,
    ) -> Option<ExprId> {
        let uses_status_function =
            if_calls_any(source, &["success", "failure", "cancelled", "always"]);
        let lowered = lower_scalar(
            &format!("${{{{ {source} }}}}"),
            span,
            site,
            at_step,
            false,
            self.b.exprs(),
            &mut self.diags,
        )?;
        let expr = match lowered {
            LoweredScalar::Expr(id) => id,
            LoweredScalar::Literal(v) => self.b.exprs().lit(v),
            LoweredScalar::Secret(_) => return None,
        };
        let truthy = builtin(self.b.exprs(), "loose_truthy", vec![expr]).ok()?;
        if uses_status_function {
            Some(truthy)
        } else {
            let success = site.status_function(self.b.exprs(), "success", at_step);
            Some(self.b.exprs().binary(BinOp::And, success, truthy))
        }
    }

    fn env_value(&mut self, node: &Node<'_>, site: &Site, at_step: bool) -> Option<EnvValue> {
        let text = match node.as_str() {
            Some(t) => t.to_string(),
            None => {
                // Non-string env values (numbers, booleans) are stringified.
                return Some(EnvValue::Plain(ExprOrValue::Value(match node.to_json() {
                    Value::String(s) => Value::String(s),
                    other => Value::String(match other {
                        Value::Null => String::new(),
                        v => v.to_string(),
                    }),
                })));
            }
        };
        match lower_scalar(
            &text,
            node.span(),
            site,
            at_step,
            true,
            self.b.exprs(),
            &mut self.diags,
        )? {
            LoweredScalar::Literal(v) => Some(EnvValue::Plain(ExprOrValue::Value(v))),
            LoweredScalar::Expr(id) => {
                // Env values are strings.
                let s = builtin(self.b.exprs(), "loose_string", vec![id]).ok()?;
                Some(EnvValue::Plain(ExprOrValue::Expr(s)))
            }
            LoweredScalar::Secret(name) => Some(EnvValue::Secret(name)),
        }
    }

    fn minutes(&mut self, node: Option<Node<'_>>) -> Option<Duration> {
        let node = node?;
        let scalar = node.as_scalar()?;
        if let Some(n) = scalar
            .as_i64()
            .map(|i| i as f64)
            .or_else(|| scalar.as_f64())
            && n > 0.0
        {
            return Some(Duration::from_secs_f64(n * 60.0));
        }
        self.diags.unsupported(
            "timeout.expression",
            node.span(),
            format!("`timeout-minutes: {}`", scalar.as_str()),
            "use a literal number of minutes",
        );
        None
    }

    /// `strategy.matrix` as an expression: a literal object with any `${{ }}` values
    /// lowered in place, or the whole thing an expression.
    fn matrix_expr(&mut self, node: Node<'_>, site: &Site) -> Option<ExprId> {
        self.yaml_expr(node, site)
    }

    fn yaml_expr(&mut self, node: Node<'_>, site: &Site) -> Option<ExprId> {
        if let Some(m) = node.as_mapping() {
            let mut fields = Vec::new();
            for (k, v) in m.iter() {
                let id = self.yaml_expr(v, site)?;
                fields.push((k.to_string(), id));
            }
            let t = self.b.exprs();
            return Some(t.object(fields.iter().map(|(k, v)| (k.as_str(), *v)).collect()));
        }
        if let Some(s) = node.as_sequence() {
            let mut items = Vec::new();
            for item in s.iter() {
                items.push(self.yaml_expr(item, site)?);
            }
            return Some(self.b.exprs().array(items));
        }
        let scalar = node.as_scalar()?;
        let text = scalar.as_str();
        if text.contains("${{") {
            return match lower_scalar(
                text,
                node.span(),
                site,
                false,
                false,
                self.b.exprs(),
                &mut self.diags,
            )? {
                LoweredScalar::Expr(id) => Some(id),
                LoweredScalar::Literal(v) => Some(self.b.exprs().lit(v)),
                LoweredScalar::Secret(_) => None,
            };
        }
        Some(self.b.exprs().lit(node.to_json()))
    }

    /// The leg count when the matrix is fully literal.
    fn static_matrix_total(&self, node: Node<'_>) -> Option<usize> {
        fn has_expr(n: Node<'_>) -> bool {
            if let Some(m) = n.as_mapping() {
                return m.iter().any(|(_, v)| has_expr(v));
            }
            if let Some(s) = n.as_sequence() {
                return s.iter().any(has_expr);
            }
            n.as_str().is_some_and(|t| t.contains("${{"))
        }
        if has_expr(node) {
            return None;
        }
        // The same composition the engine will run, evaluated here on the literal.
        let mut table = ir::ExprTable::new();
        let matrix = table.lit(node.to_json());
        let legs = crate::expr_lower::matrix_legs(&mut table, matrix).ok()?;
        let run = ir::RunContext::new();
        let statics = ir::StaticCtx::new();
        let env = ir::EvalEnv::new(&Value::Null, &run, &statics);
        ir::eval(&table, legs, &env).ok()?.as_array().map(Vec::len)
    }

    fn span_for(&self, error: &ValidationError) -> Span {
        let node = match error {
            ValidationError::LoopHeadMustJoinAny(n)
            | ValidationError::ZeroBudget(n)
            | ValidationError::UnboundedLoopBudget(n)
            | ValidationError::HirFieldInPlan(n)
            | ValidationError::EntryHasIncoming(n) => Some(*n),
            ValidationError::AlwaysNotLast { node, .. }
            | ValidationError::EmptyGroup { node, .. }
            | ValidationError::UnknownScope { node, .. }
            | ValidationError::ExitUnreachable { node, .. }
            | ValidationError::ExitNotPostdominator { node, .. }
            | ValidationError::BoundaryCrossing { node, .. } => Some(*node),
            _ => None,
        };
        node.and_then(|n| self.spans.get(&n).cloned())
            .unwrap_or_else(|| self.wf.span.clone())
    }
}

enum EnvValue {
    Plain(ExprOrValue),
    Secret(String),
}

/// A manifest's declared inputs, owned, so the plan outlives the document.
///
/// A `default:` key that is present satisfies the input, even written as `''` or
/// left empty: the YAML reader cannot tell a quoted empty string from a null
/// (see `Scalar::is_plain`), and the run-time difference — `INPUT_X` set to the
/// empty string versus unset — is invisible to the toolkit's `getInput`.
fn plan_inputs(inputs: &[composite::Input<'_>]) -> Vec<PlanInput> {
    inputs
        .iter()
        .map(|input| PlanInput {
            name: input.name.clone(),
            default: input.default.map(scalar_text),
            required: input.required,
        })
        .collect()
}

/// A YAML scalar as the string GitHub would pass: text as written, other scalars
/// stringified, null empty.
fn scalar_text(node: Node<'_>) -> String {
    match node.as_str() {
        Some(text) => text.to_string(),
        None => match node.to_json() {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        },
    }
}

/// Why an `if:` text holds no single expression. `condition` turns these into
/// diagnostics; `names_cleanup` reads them as false.
enum IfTemplateError {
    /// Text mixed with `${{ }}`, or more than one `${{ }}`.
    Mixed,
    /// An unterminated `${{`.
    Unterminated,
}

/// The one expression an `if:` string holds: the text itself, or the body of its
/// single `${{ }}`. Both readers of an `if:` go through here, so they cannot
/// disagree on what counts as one expression.
fn if_expr_source(text: &str) -> Result<String, IfTemplateError> {
    if !text.contains("${{") {
        return Ok(text.to_string());
    }
    match frontend::expr::split_template(text) {
        Ok(segments) => match segments.as_slice() {
            [frontend::expr::Segment::Expr { source, .. }] => Ok(source.clone()),
            _ => Err(IfTemplateError::Mixed),
        },
        Err(_) => Err(IfTemplateError::Unterminated),
    }
}

/// Whether the expression calls any of these functions. Parse problems read as
/// false.
fn if_calls_any(source: &str, names: &[&str]) -> bool {
    parse(source)
        .map(|ast| {
            ast.calls()
                .iter()
                .any(|c| names.contains(&c.to_lowercase().as_str()))
        })
        .unwrap_or(false)
}

/// Whether an `if:` names `always()` or `cancelled()` — the conditions GitHub
/// still honours after a cancellation, and therefore where `run_on_cancel`
/// belongs. Parse problems read as false; `condition` reports them.
fn names_cleanup(node: Option<Node<'_>>) -> bool {
    let Some(scalar) = node.and_then(|n| n.as_scalar()) else {
        return false;
    };
    if scalar.as_bool().is_some() {
        return false;
    }
    let Ok(source) = if_expr_source(scalar.as_str()) else {
        return false;
    };
    if_calls_any(&source, &["always", "cancelled"])
}

fn variant(error: &ValidationError) -> &'static str {
    match error {
        ValidationError::CycleWithoutBackEdge(_) => "cycle_without_back_edge",
        ValidationError::LoopHeadMustJoinAny(_) => "loop_head_must_join_any",
        ValidationError::ExitUnreachable { .. }
        | ValidationError::ExitNotPostdominator { .. }
        | ValidationError::BoundaryCrossing { .. } => "expansion_region",
        _ => "structure",
    }
}
