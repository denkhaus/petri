//! Jobs and workflow calls: the `start`/`done` bracket, the body between
//! them, the `needs` edges, and the summary each job's last edge carries.

use std::collections::BTreeMap;
use std::mem;

use frontend::diag::Diagnostics;
use frontend::yaml::Node;
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{BinOp, ExpandTarget, ExprId, JoinPolicy, NodeId, Scope, ScopeId, StepRef, Value};
use serde_json::{Map, json};

use super::{ActionContext, ActionPlan, Entry, EnvValue, JobNodes, Lowering, scalar_text};
use crate::action::{
    self, BACKGROUND_COMPLETE_KIND, BACKGROUND_PUBLISH_KIND, BACKGROUND_START_KIND,
    BACKGROUND_WAIT_KIND, Phase,
};
use crate::call::CalleeSource;
use crate::composite::{self, Uses};
use crate::exprs::{
    ExprSite, LoweredScalar, SEP, Site, any_status, config_value, lower_scalar, result_priority,
    status_fold, whole_value_secret,
};
use crate::model::{Defaults, Job, Step};
use crate::{expr_lower, runs_on};

struct BackgroundBranch {
    completion:      NodeId,
    completion_name: String,
    public_name:     String,
    display:         Value,
    timeout_minutes: Option<String>,
    published:       bool,
    waiters:         Vec<NodeId>,
}

impl<'w, 'a> Lowering<'w, 'a> {
    /// Inside a called workflow, every job gate carries the call's own
    /// admission: when the call was skipped or cancelled, nothing of the
    /// callee runs — not even `if: always()` — exactly as on GitHub.
    fn wrap_call_admission(&mut self, gate: Option<ExprId>, site: &Site) -> Option<ExprId> {
        let Some(call_start) = self.frame_ctx[self.current].call_start.clone() else {
            return gate;
        };
        let gate = gate?;
        let t = self.b.exprs();
        let record = site.need_record(t, &call_start);
        let status = t.field(record, "status");
        let ok = t.lit("success");
        let admitted = t.binary(BinOp::Eq, status, ok);
        Some(t.binary(BinOp::And, admitted, gate))
    }

    pub(super) fn job_shell(&mut self, job: &Job<'a>) {
        let matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        let scope = self.scope_for(job);
        // `start` records whether it fired with its scope already cancelled: a
        // cleanup job admitted after a cancel says so here, and its steps'
        // `success()` reads it back (`Site::cancel_interrupt`).
        let cancelled = self.scope_cancelled_config();
        let start = self.b.add_node(
            &format!("{}{SEP}start", job.id),
            scope,
            StepRef::new(
                "noop",
                json!({ "job": job.id, "phase": "start", "cancelled": cancelled }),
            ),
        );
        // Every node fires after a polite cancel and its gate or precondition
        // decides — except an expansion head, which a cancelled scope never
        // splices. A matrix `start` carries the expansion, so it stays unflagged,
        // and so do its clones: a leg whose start had not fired when its scope
        // was cancelled never begins, exactly as GitHub cancels a queued
        // `fail-fast` leg before any of its steps.
        if !matrix {
            self.b.node_mut(start).run_on_cancel = true;
        }
        // Frontend facts preserved on `start`: the deployment target the run
        // ignores (the reader warned; values stay as written — an ignored field's
        // expressions are never evaluated), and what each matrix leg's `runs-on`
        // resolved to.
        let mut meta = Map::new();
        if let Some(environment) = &job.environment {
            let mut target = Map::new();
            target.insert("name".into(), json!(scalar_text(environment.name)));
            if let Some(url) = environment.url {
                target.insert("url".into(), json!(scalar_text(url)));
            }
            if let Some(deployment) = environment.deployment {
                target.insert("deployment".into(), json!(scalar_text(deployment)));
            }
            meta.insert("environment".into(), Value::Object(target));
        }
        if let Some(legs) = self.leg_runs_on.take() {
            meta.insert("runs_on".into(), legs);
        }
        if !meta.is_empty() {
            self.b.set_meta(start, Value::Object(meta));
        }
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
        self.jobs.insert(job.id.clone(), JobNodes {
            scope,
            start,
            done,
            last: start,
            matrix,
        });
    }

    /// A workflow call's bracket: `start` (the caller-side gate, and the
    /// expansion head when the call has a matrix), `exit` (the join every
    /// callee job's `done` feeds), and `done` (the fold dependents read, shaped
    /// exactly like a job's so `needs.<call>.outputs.*` works unchanged). The
    /// callee's jobs land between `start` and `exit`, so a matrix call expands
    /// the whole inlined workflow per leg.
    pub(super) fn call_shell(&mut self, e: &Entry<'w, 'a>, callee: usize) {
        let job = &e.job;
        let matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        // The bracket's own scope: all three nodes are engine-side noops.
        let scope = self.b.add_scope(Scope::new(ScopeId::new(0)));
        let cancelled = self.scope_cancelled_config();
        let start = self.b.add_node(
            &format!("{}{SEP}start", job.id),
            scope,
            StepRef::new(
                "noop",
                json!({ "job": job.id, "phase": "start", "cancelled": cancelled }),
            ),
        );
        if !matrix {
            self.b.node_mut(start).run_on_cancel = true;
        }
        // What the call is, preserved on its start: the reference as written,
        // and the commit it pinned to.
        let call = job.call.as_ref().expect("a call entry");
        let mut target = Map::new();
        target.insert("uses".into(), json!(call.uses.0));
        if let CalleeSource::Remote { pinned } = &self.frames[callee].source {
            target.insert("sha".into(), json!(pinned.sha()));
        }
        self.b.set_meta(start, json!({ "call": target }));
        self.spans.insert(start, job.span.clone());

        let exit = self.b.add_node(
            &format!("{}{SEP}exit", job.id),
            scope,
            StepRef::new("noop", Value::Null),
        );
        self.b.node_mut(exit).run_on_cancel = true;
        self.spans.insert(exit, job.span.clone());
        let done = self.b.add_node(
            &format!("{}{SEP}done", job.id),
            scope,
            StepRef::new("noop", Value::Null),
        );
        self.b.node_mut(done).run_on_cancel = true;
        self.spans.insert(done, job.span.clone());
        self.jobs.insert(job.id.clone(), JobNodes {
            scope,
            start,
            done,
            last: exit,
            matrix,
        });
        self.frame_ctx[callee].exit = Some(exit);
    }

    /// The call's behavior: the caller-side gate on `start`, the summary the
    /// `exit` join carries into `done` — the callee jobs' combined result
    /// (GitHub's call conclusion) plus the declared `workflow_call.outputs`,
    /// evaluated when every callee job of the leg is complete — and the matrix
    /// expansion over the whole bracket.
    pub(super) fn call_body(&mut self, e: &Entry<'w, 'a>, callee: usize) {
        let job = &e.job;
        let Some((start, done, exit)) = self.jobs.get(&job.id).map(|j| (j.start, j.done, j.last))
        else {
            return;
        };
        let mut site = self.base_site(job);
        let items = self.apply_strategy(job, &mut site);
        let gate = self.condition(job.condition, &site, ExprSite::Job, job.span.clone());
        let gate = self.wrap_call_admission(gate, &site);
        if let Some(gate) = gate {
            self.b.set_precondition(start, gate);
        }

        // The callee's jobs, by their prefixed done nodes; the exit's site reads
        // them — suffix-aware inside a matrix call — through the `jobs` context
        // and the result terms below. The callee frame's expansion flag already
        // includes the call's own matrix.
        let callee_wf = self.frames[callee].wf;
        let mut exit_site = Site::new(&job.id);
        exit_site.in_expansion = self.frames[callee].in_expansion;
        exit_site.workflow_inputs = self.frame_ctx[callee].inputs.clone();
        exit_site.callee_jobs = Some(
            callee_wf
                .jobs
                .iter()
                .map(|j| (j.id.clone(), format!("{}{SEP}{}{SEP}done", job.id, j.id)))
                .collect(),
        );

        // Each callee job's folded result, read once; the `any_*` terms below
        // compare these shared reads against their tags. Results are already
        // folded per job ('failure'/'cancelled'/'success'/'skipped').
        let mut results = Vec::new();
        for done_name in exit_site.callee_jobs.as_ref().expect("just set").values() {
            let t = self.b.exprs();
            results.push(exit_site.need_result(t, done_name));
        }
        let any = |t: &mut ir::ExprTable, tag: &str| -> ExprId {
            let lit = t.lit(tag);
            let mut acc: Option<ExprId> = None;
            for result in &results {
                let is = t.binary(BinOp::Eq, *result, lit);
                acc = Some(match acc {
                    Some(a) => t.binary(BinOp::Or, a, is),
                    None => is,
                });
            }
            acc.unwrap_or_else(|| t.lit(false))
        };
        let t = self.b.exprs();
        let any_failure = any(t, "failure");
        let any_cancelled = any(t, "cancelled");
        let any_success = any(t, "success");
        let result = result_priority(t, any_failure, any_cancelled, any_success);

        // Declared outputs, lowered over the `jobs.*` context.
        let outputs = match &callee_wf.call {
            Some(interface) => self.lower_outputs(&interface.outputs, &exit_site, ExprSite::Job),
            None => Vec::new(),
        };
        let expanded = exit_site.in_expansion;
        let summary = self.summary_object(result, &outputs, expanded);
        self.b
            .select(exit, vec![ir::Arm::always(done).with_map(summary)]);
        let fold = self.done_config();
        self.b.node_mut(done).step = StepRef::new("noop", fold);

        // Scheduling: the callee's rootless jobs begin when the call does.
        let roots: Vec<NodeId> = callee_wf
            .jobs
            .iter()
            .filter(|j| j.needs.is_empty())
            .filter_map(|j| {
                self.jobs
                    .get(&format!("{}{SEP}{}", job.id, j.id))
                    .map(|n| n.start)
            })
            .collect();
        if !roots.is_empty() {
            self.b.fan_out(start, &roots);
        }

        if let Some(items) = items {
            ir::parallel_for_each(
                &mut self.b,
                start,
                items,
                ExpandTarget::Subgraph { entry: start, exit },
                site.max_parallel,
                site.fail_fast,
            );
        }
    }

    /// `strategy:` onto the site — `fail-fast`, `max-parallel`, the static leg
    /// count — and the expansion's items expression, shared by plain jobs and
    /// workflow calls.
    fn apply_strategy(&mut self, job: &Job<'a>, site: &mut Site) -> Option<ExprId> {
        let strategy = job.strategy.as_ref().filter(|s| s.matrix.is_some())?;
        let matrix_node = strategy
            .matrix
            .expect("the strategy was filtered to those that have a matrix");
        let matrix_expr = self.matrix_expr(matrix_node, site);
        let inputs = self.placement_inputs();
        site.matrix_total = runs_on::static_legs(matrix_node, &inputs, &self.github_identity)
            .map(|legs| legs.len());
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
            Some((Some(n), _)) if n >= 1 => match u32::try_from(n) {
                Ok(n) => Some(n),
                // Wider than the id space: take the widest cap rather than
                // wrapping to a narrow one.
                Err(_) => Some(u32::MAX),
            },
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
        matrix_expr.and_then(|m| expr_lower::matrix_legs(self.b.exprs(), m).ok())
    }

    /// Steps, preconditions, config, and the matrix expansion.
    pub(super) fn job_body(&mut self, job: &Job<'a>) {
        let shell = self
            .jobs
            .get(&job.id)
            .map(|j| (j.scope, j.start, j.done, j.matrix));
        let Some((scope, start, done, matrix)) = shell else {
            return;
        };

        // Secrets at container/workflow/job level are pushed down into every step.
        let mut site = self.base_site(job);
        let mut job_secret_env: Vec<(String, String)> = Vec::new();
        for (key, node) in super::scope::scope_env(self.wf, job) {
            // Lowered once already for the scope; here only to find the secrets, whose
            // diagnostics (if any) were reported then.
            let mut scratch = Diagnostics::new();
            let saved = mem::replace(&mut self.diags, scratch);
            let value = self.env_value(&node, &site, ExprSite::Job);
            scratch = mem::replace(&mut self.diags, saved);
            let _ = scratch;
            if let Some(EnvValue::Secret(name)) = value {
                job_secret_env.push((key, name));
            }
        }
        let _ = matrix;

        // Matrix.
        let matrix_items = self.apply_strategy(job, &mut site);
        let job_environment = self.job_environment_channel(&site);

        // The gate: needs + the job's own if — and, inside a called workflow,
        // the call's own admission.
        let gate = self.condition(job.condition, &site, ExprSite::Job, job.span.clone());
        let gate = self.wrap_call_admission(gate, &site);
        if let Some(gate) = gate {
            self.b.set_precondition(start, gate);
        }

        // Steps. Executable steps have public names; wait controls do not.
        let mut names_so_far: Vec<String> = Vec::new();
        let mut step_names: BTreeMap<String, String> = BTreeMap::new();
        for step in job
            .steps
            .iter()
            .filter(|step| step.run.is_some() || step.uses.is_some())
        {
            step_names.insert(
                step.node_name(),
                format!("{}{SEP}{}", job.id, step.node_name()),
            );
        }
        site.step_names = step_names;

        // A JavaScript or Docker action with `pre` or `post` contributes nodes
        // away from its own position: GitHub runs every `pre` before the first
        // step and every `post` after the last, in reverse order. Resolved once,
        // quietly; the main pass reports whatever is wrong with the reference.
        let plans: Vec<Option<ActionPlan>> = job
            .steps
            .iter()
            .map(|step| {
                // A substituted checkout has no plan: the real action's `post`
                // (credential cleanup) belongs to the action that never runs.
                if self.substitutable_checkout(step).is_some() {
                    return None;
                }
                self.action_plan(&job.id, step)
            })
            .collect();

        let mut previous = start;
        let mut chain: Vec<NodeId> = Vec::new();
        let mut routes: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        let mut backgrounds: BTreeMap<String, BackgroundBranch> = BTreeMap::new();
        let mut background_order = Vec::new();
        let mut wait_index = 0usize;
        for (step, plan) in job.steps.iter().zip(&plans) {
            if let Some(plan) = plan
                && plan.has_pre()
                && let Some(id) = self.lifecycle_node(
                    ActionContext {
                        job,
                        step,
                        scope,
                        site: &site,
                        job_secret_env: &job_secret_env,
                    },
                    plan,
                    Phase::Pre,
                )
            {
                self.configure_job_environment(id, job_environment.as_ref());
                self.chain_node(
                    &mut routes,
                    &mut previous,
                    &mut chain,
                    &mut names_so_far,
                    &mut site,
                    id,
                );
            }
        }
        for (step, plan) in job.steps.iter().zip(&plans) {
            if step.is_wait() {
                let targets = if step.wait_all {
                    background_order
                        .iter()
                        .filter(|name| {
                            backgrounds
                                .get(*name)
                                .is_some_and(|branch| !branch.published)
                        })
                        .cloned()
                        .collect()
                } else {
                    step.wait.clone().unwrap_or_default()
                };
                self.background_wait(
                    job,
                    step,
                    scope,
                    &mut site,
                    &mut routes,
                    &mut previous,
                    &mut chain,
                    &mut names_so_far,
                    &mut backgrounds,
                    &targets,
                    wait_index,
                );
                wait_index += 1;
                continue;
            }
            let inherited = Defaults {
                shell:             step.shell.or(job.defaults.shell).or(self.wf.defaults.shell),
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
                plan.as_ref(),
            );
            for id in &nodes {
                self.configure_job_environment(*id, job_environment.as_ref());
            }
            if step.background {
                if nodes.is_empty() {
                    continue;
                }
                let key = step.node_name();
                let channel = self.background_channel(&site, background_order.len());
                self.launch_background(
                    job,
                    step,
                    scope,
                    &mut site,
                    &mut routes,
                    &mut previous,
                    &mut chain,
                    &nodes,
                    &channel,
                    job_environment.as_ref(),
                    &mut backgrounds,
                );
                background_order.push(key);
            } else {
                for id in nodes {
                    self.chain_node(
                        &mut routes,
                        &mut previous,
                        &mut chain,
                        &mut names_so_far,
                        &mut site,
                        id,
                    );
                }
            }
        }

        // GitHub inserts this barrier before action post phases. It publishes
        // only targets no earlier wait-all (or first explicit wait) completed.
        let implicit_targets: Vec<String> = background_order
            .iter()
            .filter(|name| {
                backgrounds
                    .get(*name)
                    .is_some_and(|branch| !branch.published)
            })
            .cloned()
            .collect();
        if !implicit_targets.is_empty() {
            let implicit = Step {
                implicit_id:       "implicit-wait-all".into(),
                id:                None,
                name:              None,
                span:              job.span.clone(),
                condition:         None,
                run:               None,
                uses:              None,
                with:              Vec::new(),
                env:               Vec::new(),
                shell:             None,
                working_directory: None,
                continue_on_error: None,
                timeout_minutes:   None,
                background:        false,
                wait:              None,
                wait_all:          true,
            };
            self.background_wait(
                job,
                &implicit,
                scope,
                &mut site,
                &mut routes,
                &mut previous,
                &mut chain,
                &mut names_so_far,
                &mut backgrounds,
                &implicit_targets,
                wait_index,
            );
        }

        for (step, plan) in job.steps.iter().zip(&plans).rev() {
            let eager = self.is_eager_root(&job.id, &step.node_name());
            let deferred = !eager
                && plan.is_none()
                && step.uses.as_ref().is_some_and(|(reference, _)| {
                    !matches!(composite::classify(reference), Uses::Docker(_))
                });
            if deferred
                && let Some(id) = self.deferred_action_post_node(ActionContext {
                    job,
                    step,
                    scope,
                    site: &site,
                    job_secret_env: &job_secret_env,
                })
            {
                self.chain_node(
                    &mut routes,
                    &mut previous,
                    &mut chain,
                    &mut names_so_far,
                    &mut site,
                    id,
                );
            } else if let Some(plan) = plan
                && plan.has_post()
                && let Some(id) = self.lifecycle_node(
                    ActionContext {
                        job,
                        step,
                        scope,
                        site: &site,
                        job_secret_env: &job_secret_env,
                    },
                    plan,
                    Phase::Post,
                )
            {
                self.configure_job_environment(id, job_environment.as_ref());
                self.chain_node(
                    &mut routes,
                    &mut previous,
                    &mut chain,
                    &mut names_so_far,
                    &mut site,
                    id,
                );
            }
        }

        // A background completion may feed several explicit waits. The one
        // fan-out preserves a completion token for each join.
        for branch in backgrounds.values() {
            match branch.waiters.as_slice() {
                [] => {}
                [only] => {
                    self.b.link(branch.completion, *only);
                }
                many => {
                    self.b.fan_out(branch.completion, many);
                }
            }
        }
        for (from, targets) in routes {
            match targets.as_slice() {
                [] => {}
                [only] => {
                    self.b.link(from, *only);
                }
                many => {
                    self.b.fan_out(from, many);
                }
            }
        }

        let last = previous;
        if let Some(j) = self.jobs.get_mut(&job.id) {
            j.last = last;
        }

        // Job-level `continue-on-error`: the job's failure is control flow. Its
        // nodes' failures no longer fail the run or trigger fail-fast, and the
        // summary below reports `success` to dependents — as GitHub's does.
        let tolerated = self.job_tolerates_failure(job);
        if tolerated {
            for id in [start, done].into_iter().chain(chain.iter().copied()) {
                self.b.node_mut(id).tolerates_failure = true;
            }
        }

        // The last step's edge carries the job summary into `done`. It is evaluated in
        // the last step's outcome context, so `status` is that step's own.
        let summary = self.job_summary(job, &site, last == start, tolerated);
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
                    exit:  last,
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

    /// `done` → each dependent's `start`, within the entry's own frame — and,
    /// inside a called workflow, → the call's exit join, which counts every
    /// callee job.
    pub(super) fn job_edges(&mut self, e: &Entry<'w, 'a>, entries: &[Entry<'w, 'a>]) {
        let job = &e.job;
        let Some(done) = self.jobs.get(&job.id).map(|j| j.done) else {
            return;
        };
        let mut targets: Vec<NodeId> = entries
            .iter()
            .filter(|other| other.frame == e.frame)
            .filter(|other| other.job.needs.iter().any(|(n, _)| n == &job.id))
            .filter_map(|other| self.jobs.get(&other.job.id).map(|j| j.start))
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
        if let Some(exit) = self.frame_ctx[e.frame].exit {
            targets.push(exit);
        }
        if !targets.is_empty() {
            self.b.fan_out(done, &targets);
        }
    }

    fn background_channel(&mut self, site: &Site, ordinal: usize) -> Value {
        let base = format!("background-{}", ordinal + 1);
        if !(site.matrix || site.in_expansion) {
            return json!(base);
        }
        let t = self.b.exprs();
        let prefix = t.lit(format!("{base}#"));
        let index = t.var("index");
        let key = t.binary(BinOp::Add, prefix, index);
        json!({ EXPR_PLACEHOLDER_KEY: key.raw() })
    }

    fn job_environment_channel(&mut self, site: &Site) -> Option<Value> {
        if !(site.matrix || site.in_expansion) {
            return None;
        }
        let t = self.b.exprs();
        let prefix = t.lit("leg#");
        let index = t.var("index");
        let key = t.binary(BinOp::Add, prefix, index);
        Some(json!({ EXPR_PLACEHOLDER_KEY: key.raw() }))
    }

    fn configure_job_environment(&mut self, id: NodeId, channel: Option<&Value>) {
        let Some(channel) = channel else { return };
        let node = self.b.node_mut(id);
        if action::channel_bearing(node.step.kind.as_ref())
            && let Value::Object(config) = &mut node.step.config
        {
            config.insert("job_environment".into(), channel.clone());
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "background lowering carries the job's graph, expression, and branch state together"
    )]
    fn launch_background(
        &mut self,
        job: &Job<'a>,
        step: &Step<'a>,
        scope: ScopeId,
        site: &mut Site,
        routes: &mut BTreeMap<NodeId, Vec<NodeId>>,
        previous: &mut NodeId,
        chain: &mut Vec<NodeId>,
        nodes: &[NodeId],
        channel: &Value,
        job_environment: Option<&Value>,
        backgrounds: &mut BTreeMap<String, BackgroundBranch>,
    ) {
        let public_name = format!("{}{SEP}{}", job.id, step.node_name());
        let start_name = format!("{public_name}{SEP}background-start");
        let start = self.b.add_node(
            &start_name,
            scope,
            StepRef::new(
                BACKGROUND_START_KIND,
                json!({ "key": channel, "job_environment": job_environment }),
            ),
        );
        self.b.node_mut(start).run_on_cancel = true;
        self.b.node_mut(start).tolerates_failure = true;
        self.spans.insert(start, step.span.clone());
        routes.entry(*previous).or_default().push(start);
        *previous = start;
        chain.push(start);
        for id in nodes {
            let node = self.b.node_mut(*id);
            node.run_on_cancel = true;
            node.tolerates_failure = true;
            if node.name == public_name {
                node.name = format!("{public_name}{SEP}background").into();
            }
            if action::channel_bearing(node.step.kind.as_ref())
                && let Value::Object(config) = &mut node.step.config
            {
                config.insert("background".into(), channel.clone());
            }
            chain.push(*id);
        }
        routes.entry(start).or_default().push(nodes[0]);
        for pair in nodes.windows(2) {
            routes.entry(pair[0]).or_default().push(pair[1]);
        }

        let node_names: Vec<String> = nodes
            .iter()
            .filter_map(|id| self.b.graph().node(*id).map(|node| node.name.to_string()))
            .collect();
        let worker_status = self.background_status(site, &node_names);
        let start_failed = any_status(
            self.b.exprs(),
            &[start_name],
            &["failure", "timed_out"],
            site.expanded(),
        );
        let failure = self.b.exprs().lit("failure");
        let status = self.b.exprs().cond(start_failed, failure, worker_status);
        let output = if let Some(outputs) = site.composite_outputs.remove(&step.node_name()) {
            let entries: Vec<_> = outputs
                .iter()
                .map(|(name, id)| (name.as_str(), *id))
                .collect();
            self.b.exprs().object(entries)
        } else {
            let record = site.node_record(
                self.b.exprs(),
                node_names.last().expect("a background has a terminal"),
            );
            self.b.exprs().field(record, "output")
        };
        let completion_name = format!("{public_name}{SEP}background-done");
        let completion = self.b.add_node(
            &completion_name,
            scope,
            StepRef::new(
                BACKGROUND_COMPLETE_KIND,
                json!({
                    "status": { EXPR_PLACEHOLDER_KEY: status.raw() },
                    "output": { EXPR_PLACEHOLDER_KEY: output.raw() },
                    "key": channel,
                    "job_environment": job_environment,
                }),
            ),
        );
        self.b.node_mut(completion).run_on_cancel = true;
        self.b.node_mut(completion).tolerates_failure = true;
        self.spans.insert(completion, step.span.clone());
        chain.push(completion);
        routes
            .entry(*nodes.last().expect("a background has a terminal"))
            .or_default()
            .push(completion);

        backgrounds.insert(step.node_name(), BackgroundBranch {
            completion,
            completion_name,
            public_name,
            display: self.background_display(step, site),
            timeout_minutes: step
                .timeout_minutes
                .and_then(|value| value.as_scalar())
                .map(|value| value.as_str().to_string()),
            published: false,
            waiters: Vec::new(),
        });
    }

    fn background_status(&mut self, site: &Site, names: &[String]) -> ExprId {
        if let [name] = names {
            return site.node_status(self.b.exprs(), name);
        }
        status_fold(self.b.exprs(), names, site.expanded())
    }

    fn background_display(&mut self, step: &Step<'a>, site: &Site) -> Value {
        if let Some(name) = step.name
            && let Some(text) = name.as_str()
            && let Some(value) = lower_scalar(
                text,
                name.span(),
                site,
                ExprSite::Step,
                false,
                self.b.exprs(),
                &mut self.diags,
            )
        {
            return config_value(value);
        }
        if let Some((reference, _)) = &step.uses {
            return json!(reference);
        }
        json!(
            step.run
                .and_then(|run| run.as_str())
                .and_then(|run| run.lines().next())
                .filter(|line| !line.is_empty())
                .unwrap_or("Run")
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a wait joins graph state and updates the foreground expression site"
    )]
    fn background_wait(
        &mut self,
        job: &Job<'a>,
        step: &Step<'a>,
        scope: ScopeId,
        site: &mut Site,
        routes: &mut BTreeMap<NodeId, Vec<NodeId>>,
        previous: &mut NodeId,
        chain: &mut Vec<NodeId>,
        names: &mut Vec<String>,
        backgrounds: &mut BTreeMap<String, BackgroundBranch>,
        targets: &[String],
        wait_index: usize,
    ) {
        let mut verdicts = Vec::new();
        for (target_index, target) in targets.iter().enumerate() {
            let Some(branch) = backgrounds.get_mut(target) else {
                continue;
            };
            let first_publication = !branch.published;
            branch.published = true;
            let completion_name = branch.completion_name.clone();
            let public_name = branch.public_name.clone();
            let display = branch.display.clone();
            let timeout_minutes = branch.timeout_minutes.clone();

            let decision_name = format!(
                "{}{SEP}__background-wait-{}-{}-decision",
                job.id,
                wait_index + 1,
                target_index + 1
            );
            let decision = self.b.add_step(&decision_name, scope, "noop");
            self.b.node_mut(decision).run_on_cancel = true;
            routes.entry(*previous).or_default().push(decision);
            branch.waiters.push(decision);
            chain.push(decision);

            let result = {
                let record = site.node_record(self.b.exprs(), &completion_name);
                self.b.exprs().field(record, "output")
            };
            let result_status = self.b.exprs().field(result, "status");
            let skipped = self.b.exprs().lit("skipped");
            let publish_guard = self.b.exprs().binary(BinOp::Ne, result_status, skipped);
            let publish_name = if first_publication {
                public_name
            } else {
                format!(
                    "{}{SEP}__background-wait-{}-{}-publish",
                    job.id,
                    wait_index + 1,
                    target_index + 1
                )
            };
            let publish = self.b.add_node(
                &publish_name,
                scope,
                StepRef::new(
                    BACKGROUND_PUBLISH_KIND,
                    json!({ "result": { EXPR_PLACEHOLDER_KEY: result.raw() } }),
                ),
            );
            self.b.node_mut(publish).run_on_cancel = true;
            self.b.node_mut(publish).tolerates_failure = true;
            let bypass_name = format!(
                "{}{SEP}__background-wait-{}-{}-bypass",
                job.id,
                wait_index + 1,
                target_index + 1
            );
            let bypass = self.b.add_step(&bypass_name, scope, "noop");
            self.b.node_mut(bypass).run_on_cancel = true;
            let merge_name = format!(
                "{}{SEP}__background-wait-{}-{}-merge",
                job.id,
                wait_index + 1,
                target_index + 1
            );
            let merge = self.b.add_step(&merge_name, scope, "noop");
            self.b.node_mut(merge).run_on_cancel = true;
            self.b.set_join(merge, JoinPolicy::Any);
            self.b.select(decision, vec![
                ir::Arm::when(publish, publish_guard),
                ir::Arm::always(bypass),
            ]);
            self.b.link(publish, merge);
            self.b.link(bypass, merge);
            chain.extend([publish, bypass, merge]);
            *previous = merge;

            let published_status = site.node_status(self.b.exprs(), &publish_name);
            let status = self
                .b
                .exprs()
                .call("default", vec![published_status, result_status]);
            verdicts.push(json!({
                "name": display,
                "status": { EXPR_PLACEHOLDER_KEY: status.raw() },
                "timeout_minutes": timeout_minutes,
            }));
        }

        let mut config = Map::new();
        config.insert("targets".into(), Value::Array(verdicts));
        if let Some(value) = self.soft_fail_value(step.continue_on_error, site, false) {
            config.insert("continue_on_error".into(), value);
        }
        let wait_name = format!("{}{SEP}__background-wait-{}", job.id, wait_index + 1);
        let wait = self.b.add_node(
            &wait_name,
            scope,
            StepRef::new(BACKGROUND_WAIT_KIND, Value::Object(config)),
        );
        self.b.node_mut(wait).run_on_cancel = true;
        self.spans.insert(wait, step.span.clone());
        routes.entry(*previous).or_default().push(wait);
        chain.push(wait);
        *previous = wait;
        names.push(wait_name);
        site.earlier_steps.clone_from(names);
    }

    /// Link `id` after `previous` and make it visible to the steps after it.
    /// Every chained node runs on cancel: after a polite cancel it fires and
    /// its gate decides, which is the whole cancellation story (no
    /// admission sniffing). Matrix templates pass the flag to their clones.
    fn chain_node(
        &mut self,
        routes: &mut BTreeMap<NodeId, Vec<NodeId>>,
        previous: &mut NodeId,
        chain: &mut Vec<NodeId>,
        names: &mut Vec<String>,
        site: &mut Site,
        id: NodeId,
    ) {
        self.b.node_mut(id).run_on_cancel = true;
        routes.entry(*previous).or_default().push(id);
        chain.push(id);
        *previous = id;
        let name = self
            .b
            .graph()
            .node(id)
            .map(|n| n.name.to_string())
            .unwrap_or_default();
        names.push(name);
        site.earlier_steps.clone_from(names);
    }

    /// Job-level `continue-on-error`, read once per job: a literal `true`
    /// tolerates the job's failure. GitHub keeps the steps' own outcomes as
    /// they fell — within the job, `failure()` and `steps.<id>.outcome` still
    /// see them — so this masks nothing step-side.
    fn job_tolerates_failure(&mut self, job: &Job<'a>) -> bool {
        let Some(coe) = job.continue_on_error else {
            return false;
        };
        let Some(tolerated) = coe.as_scalar().and_then(|s| s.as_bool()) else {
            self.diags.unsupported(
                "continue_on_error.expression",
                coe.span(),
                "an expression-valued `continue-on-error`",
                "use a literal true or false",
            );
            return false;
        };
        tolerated
    }

    /// The `{ result, outputs }` summary the last step's edge carries into
    /// `done`, evaluated in that step's outcome context. A `tolerated` job
    /// reports `success` where it would report `failure` — GitHub's
    /// conclusion for a `continue-on-error` job, and the value
    /// `needs.<job>.result` carries — while a cancel still reports
    /// `cancelled`.
    fn job_summary(
        &mut self,
        job: &Job<'a>,
        site: &Site,
        no_steps: bool,
        tolerated: bool,
    ) -> ExprId {
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
        let start_cancelled = site.start_cancelled(t);

        let s_failure = t.lit("failure");
        let s_cancelled = t.lit("cancelled");
        let s_success = t.lit("success");
        let s_skipped = t.lit("skipped");
        let inner = t.cond(cancelled, s_cancelled, s_success);
        let ran = if tolerated {
            inner
        } else {
            t.cond(failed, s_failure, inner)
        };
        // A job that never began: cancelled before its start could fire, else
        // skipped by its own gate — the distinction GitHub reports.
        let never_ran = t.cond(start_cancelled, s_cancelled, s_skipped);
        let result = t.cond(started, ran, never_ran);

        // Job outputs, lowered where every step is visible. An output that would
        // carry a secret is dropped with a warning, as GitHub drops it.
        let mut kept: Vec<(String, Node<'_>)> = Vec::new();
        for (name, node) in &job.outputs {
            if whole_value_secret(node.as_str().unwrap_or("")).is_some() {
                self.diags.warning(
                    "ignored.secret_output",
                    node.span(),
                    format!(
                        "job output `{name}` would carry a secret; GitHub drops such an output, and so does this run"
                    ),
                );
                continue;
            }
            kept.push((name.clone(), *node));
        }
        let outputs = self.lower_outputs(&kept, site, ExprSite::Step);
        self.summary_object(result, &outputs, site.matrix)
    }

    /// Lower `name: value` output entries over `site`. A `Secret` never lands
    /// here — the caller's rules dropped or rejected it — and is skipped
    /// defensively.
    fn lower_outputs(
        &mut self,
        entries: &[(String, Node<'_>)],
        site: &Site,
        at: ExprSite,
    ) -> Vec<(String, ExprId)> {
        let mut outputs = Vec::new();
        for (name, node) in entries {
            let text = node.as_str().unwrap_or("");
            if let Some(lowered) = lower_scalar(
                text,
                node.span(),
                site,
                at,
                false,
                self.b.exprs(),
                &mut self.diags,
            ) {
                let id = match lowered {
                    LoweredScalar::Literal(v) => self.b.exprs().lit(v),
                    LoweredScalar::Expr(id) => id,
                    LoweredScalar::Secret(_) => continue,
                };
                outputs.push((name.clone(), id));
            }
        }
        outputs
    }

    /// The `{ result, outputs, index }` summary a job's — or a call's — last
    /// edge carries into `done`, where [`Lowering::done_config`] folds one per
    /// leg into the record dependents read.
    fn summary_object(
        &mut self,
        result: ExprId,
        outputs: &[(String, ExprId)],
        expanded: bool,
    ) -> ExprId {
        let t = self.b.exprs();
        let outputs_obj = t.object(outputs.iter().map(|(k, v)| (k.as_str(), *v)).collect());
        let index = if expanded { t.var("index") } else { t.lit(0) };
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
        let result = result_priority(t, any_failure, any_cancelled, any_success);

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

    /// `strategy.matrix` as an expression: a literal object with any `${{ }}`
    /// values lowered in place, or the whole thing an expression.
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
                ExprSite::Job,
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
}
