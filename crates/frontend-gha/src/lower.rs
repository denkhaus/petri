//! Workflow → HIR, per spec §12.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use frontend::diag::{Diagnostic, Diagnostics, Lowered, Span};
use frontend::expr::lower::builtin;
use frontend::expr::parse;
use frontend::yaml::Node;
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    BinOp, Budget, ExpandTarget, ExprId, ExprOrValue, GraphBuilder, NodeId, RuntimeSpec, Scope,
    ScopeId, StepRef, ValidationError, ValidationWarning, Value,
};
use serde_json::{Map, json};
use smol_str::SmolStr;
use steps::{NOOP_KIND, PROCESS_KIND};

use crate::FileSource;
use crate::composite::{self, Uses};
use crate::exprs::{LoweredScalar, SEP, Site, config_value, lower_scalar};
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
    jobs: HashMap<String, JobNodes>,
    spans: HashMap<NodeId, Span>,
}

pub fn lower(wf: &Workflow<'_>, files: &dyn FileSource, diags: Diagnostics) -> Lowered {
    let mut lw = Lowering {
        b: GraphBuilder::bare(),
        diags,
        wf,
        files,
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
        let span = match warning {
            ValidationWarning::ScopeReentry { at, .. } => {
                lw.spans.get(at).cloned().unwrap_or_default()
            }
        };
        lw.diags.push(
            Diagnostic::warning("lint.scope_reentry", span, warning.to_string())
                .with_hint("this lint is a static over-approximation"),
        );
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
            StepRef::new(NOOP_KIND, json!({ "job": job.id, "phase": "start" })),
        );
        self.spans.insert(start, job.span.clone());
        let done = self.b.add_node(
            &format!("{}{SEP}done", job.id),
            scope,
            StepRef::new(NOOP_KIND, Value::Null),
        );
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
            "workspace",
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

        let mut previous = start;
        let mut chain: Vec<NodeId> = Vec::new();
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
                self.b.link(previous, id);
                chain.push(id);
                previous = id;
                let name = self
                    .b
                    .graph()
                    .node(id)
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                names_so_far.push(name);
                site.earlier_steps = names_so_far.clone();
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
        self.b.node_mut(done).step = StepRef::new(NOOP_KIND, fold);

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

    /// The node(s) for one step: one process node, or a composite's inlined chain.
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
            return self.composite_step(
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
        // Step env is visible to the step's own expressions.
        let mut env_config = Map::new();
        for (key, name) in job_secret_env {
            env_config.insert(
                key.clone(),
                json!({ ir::placeholder::SECRET_REF_KEY: name }),
            );
        }
        for (key, node) in &step.env {
            match self.env_value(node, &step_site, true) {
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
            false,
            self.b.exprs(),
            &mut self.diags,
        ) {
            Some(v) => {
                config.insert("run".into(), config_value(v));
            }
            None => return Vec::new(),
        }

        // Shell.
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
            other => {
                let kind = other.split_whitespace().next().unwrap_or(other);
                self.diags.unsupported(
                    &format!(
                        "shell.{}",
                        kind.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                    ),
                    inherited.shell.map(|n| n.span()).unwrap_or_default(),
                    format!("`shell: {other}`"),
                    "only `bash` and `sh` are available in v1",
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
        config.insert("output_env_aliases".into(), json!(["GITHUB_OUTPUT"]));

        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(PROCESS_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());

        // Timeout: step, else job, else GitHub's default.
        let timeout = self
            .minutes(step.timeout_minutes)
            .or_else(|| self.minutes(job.timeout_minutes))
            .unwrap_or(DEFAULT_TIMEOUT);
        self.b.set_budget(id, Budget::new(1, timeout));

        // Precondition: the job started, and the step's own condition (default
        // `success()` over earlier steps).
        let cond = self.condition(step.condition, &step_site, true, step.span.clone());
        let started = step_site.job_started(self.b.exprs());
        let pre = match cond {
            Some(c) => self.b.exprs().binary(BinOp::And, started, c),
            None => started,
        };
        self.b.set_precondition(id, pre);
        vec![id]
    }

    /// `uses:` — a local composite is inlined; anything else is rejected with the
    /// action named, so the corpus can count what package 04 should build first.
    #[allow(clippy::too_many_arguments)]
    fn composite_step(
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
        let path = match composite::classify(reference) {
            Uses::Local(path) => path,
            Uses::Docker(image) => {
                self.diags.unsupported(
                    "action.docker",
                    span.clone(),
                    format!("`uses: docker://{image}`"),
                    "Docker actions are v2",
                );
                return Vec::new();
            }
            Uses::Remote(name) => {
                self.diags.unsupported(
                    "action.remote",
                    span.clone(),
                    name.to_string(),
                    "remote actions need the action shim layer (package 04); only `./local` composites run today",
                );
                return Vec::new();
            }
        };
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
        let Some(doc) = composite::read_document(self.files, &path, span, &mut self.diags) else {
            return Vec::new();
        };
        let Some(action) = composite::read(&doc, span, &mut self.diags) else {
            return Vec::new();
        };

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
                        Some(LoweredScalar::Secret(_)) => {
                            self.diags.unsupported(
                                "action.secret_input",
                                n.span(),
                                format!("a secret passed as composite input `{}`", input.name),
                                "composite inputs are substituted into expressions; pass the secret through `env:` on the step instead",
                            );
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

        let mut names_so_far: Vec<String> = earlier.to_vec();
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
        let text = scalar.as_str();
        let source = if text.contains("${{") {
            match frontend::expr::split_template(text) {
                Ok(segments) => match segments.as_slice() {
                    [frontend::expr::Segment::Expr { source, .. }] => source.clone(),
                    _ => {
                        self.diags.error(
                            "expr.mixed_condition",
                            node.span(),
                            "an `if` is one expression, bare or in a single `${{ }}`",
                        );
                        return None;
                    }
                },
                Err(_) => {
                    self.diags
                        .error("expr.unterminated", node.span(), "unterminated `${{`");
                    return None;
                }
            }
        } else {
            text.to_string()
        };
        let uses_status_function = parse(&source)
            .map(|ast| {
                ast.calls().iter().any(|c| {
                    matches!(
                        c.to_lowercase().as_str(),
                        "success" | "failure" | "cancelled" | "always"
                    )
                })
            })
            .unwrap_or(false);
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
