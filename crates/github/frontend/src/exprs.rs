//! What GitHub's contexts mean in engine terms.
//!
//! An expression in a workflow file sits somewhere — a job's `if:`, a step's
//! `run:`, a composite action's output — and what `steps.build.outputs.x` or
//! `success()` means depends on where. [`Site`] is that "where". [`GhaRoots`]
//! turns a context reference into an engine expression for it, and the whole
//! thing rides on the ordinary GHA lowering: there is no evaluator here, only
//! construction.

use std::collections::BTreeMap;
use std::convert::Infallible;

use frontend::diag::{Diagnostics, Span};
use frontend::expr::lower::{LowerError, Roots, builtin};
use frontend::expr::{Expr, Literal, Segment, parse, split_template};
use ir::placeholder::{EXPR_PLACEHOLDER_KEY, SECRET_REF_KEY};
use ir::{BinOp, ExprId, ExprOrValue, ExprTable, UnOp, Value};
use serde_json::json;

use crate::expr_lower::gha;

/// The node-name separator between a job and its steps, and between a composite
/// caller and its inner steps. Node names are `job/step`, `job/caller/inner`.
pub(crate) const SEP: char = '/';

/// A GitHub status tag as the engine records it, mapped to how GitHub reports
/// it. The engine has `partial_success` and `timed_out`; GitHub has neither.
pub(crate) fn outcome_tag(table: &mut ExprTable, status: ExprId) -> ExprId {
    // outcome: the result before continue-on-error — partial_success reads as
    // failure.
    remap_status(table, status, &[
        ("partial_success", "failure"),
        ("timed_out", "failure"),
    ])
}

pub(crate) fn conclusion_tag(table: &mut ExprTable, status: ExprId) -> ExprId {
    // conclusion: after continue-on-error — partial_success reads as success.
    remap_status(table, status, &[
        ("partial_success", "success"),
        ("timed_out", "failure"),
    ])
}

fn remap_status(table: &mut ExprTable, status: ExprId, pairs: &[(&str, &str)]) -> ExprId {
    let mut acc = status;
    for (from, to) in pairs.iter().rev() {
        let from_lit = table.lit(*from);
        let is = table.binary(BinOp::Eq, status, from_lit);
        let to_lit = table.lit(*to);
        acc = table.cond(is, to_lit, acc);
    }
    acc
}

/// How `secrets.NAME` maps to the provider's names at this site.
///
/// The map is a pure rename decided at lowering: no secret value — or even a
/// provider name the caller did not grant — reaches a callee's expressions.
/// `Inherit` is the root and `secrets: inherit`; `Explicit` is a call's
/// `secrets:` block, where a declared-but-not-provided optional secret reads as
/// the empty string (as on GitHub) and an undeclared name is a diagnostic.
#[derive(Clone, Default)]
pub(crate) enum SecretMap {
    /// Names pass through unchanged.
    #[default]
    Inherit,
    /// Callee name (lowercased) → the provider's name, or `None` for a declared
    /// optional secret the caller did not provide.
    Explicit(BTreeMap<String, Option<String>>),
}

/// The name is not one this workflow call granted.
pub(crate) struct UndeclaredSecret;

impl SecretMap {
    /// The provider name for a callee's `secrets.NAME`: `Ok(Some)` to
    /// reference, `Ok(None)` for a declared-but-absent optional (the value
    /// is empty), and `Err` for a name this call never granted.
    /// `GITHUB_TOKEN` is the runner's token, not a caller-granted secret:
    /// it crosses every call boundary unmapped, as on GitHub.
    pub(crate) fn resolve(&self, name: &str) -> Result<Option<String>, UndeclaredSecret> {
        if name.eq_ignore_ascii_case(GITHUB_TOKEN_SECRET) {
            return Ok(Some(GITHUB_TOKEN_SECRET.to_string()));
        }
        match self {
            Self::Inherit => Ok(Some(name.to_string())),
            Self::Explicit(map) => match map.get(&name.to_lowercase()) {
                Some(provider) => Ok(provider.clone()),
                None => Err(UndeclaredSecret),
            },
        }
    }
}

/// Which site an expression lowers at: a step's position (`if:`, `run:`,
/// `env:`, `with:`) or a job's (`if:`, `env:`, outputs). Step positions see
/// the job's earlier steps and the runner-side sentinels; job positions see
/// the needed jobs. [`Site`] carries the surroundings; this says which of the
/// two readings of them applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExprSite {
    Step,
    Job,
}

/// Where an expression sits.
#[derive(Clone)]
pub(crate) struct Site {
    pub job_id:            String,
    /// `job/step` names of the steps before this one in the job, in order.
    pub earlier_steps:     Vec<String>,
    /// Every step of this job, for `steps.<id>` lookups: id → node name.
    pub step_names:        BTreeMap<String, String>,
    /// The job's `needs`, with each needed job's `done` node name.
    pub needs:             BTreeMap<String, String>,
    /// The job is a matrix job, so node names take a `#index` suffix at run
    /// time.
    pub matrix:            bool,
    /// Static matrix leg count, when the matrix is a literal.
    pub matrix_total:      Option<usize>,
    pub fail_fast:         bool,
    pub max_parallel:      Option<u32>,
    /// Step-level `env`, for `env.X` inside the step.
    pub step_env:          BTreeMap<String, ExprOrValue>,
    /// Composite action inputs in scope: input name → already-lowered value.
    pub action_inputs:     Option<BTreeMap<String, ExprId>>,
    /// Workflow-scope inputs — a called workflow's bound `with:`, or a directly
    /// run workflow's typed run-parameter reads. `action_inputs` shadows this
    /// inside a composite body, where `inputs` means the action's own.
    pub workflow_inputs:   Option<BTreeMap<String, ExprId>>,
    /// How `secrets.*` names map to the provider's at this site.
    pub secrets:           SecretMap,
    /// Inside `on.workflow_call.outputs` values: callee job id → its `done`
    /// node name, for the `jobs.<id>.*` context. `None` anywhere else — the
    /// context is out of scope there, even for a callee with no jobs.
    pub callee_jobs:       Option<BTreeMap<String, String>>,
    /// The site sits inside an expansion region without being a matrix job
    /// itself — a called workflow's job under a matrix call. Node names take
    /// the `#index` suffix, but the `matrix` context stays empty.
    pub in_expansion:      bool,
    /// Composite action outputs the caller can see: step id → (output name →
    /// expr).
    pub composite_outputs: BTreeMap<String, BTreeMap<String, ExprId>>,
    /// The job's own `start` node name (its gate).
    pub start_node:        String,
}

impl Site {
    pub(crate) fn new(job_id: &str) -> Self {
        Self {
            job_id:            job_id.to_string(),
            earlier_steps:     Vec::new(),
            step_names:        BTreeMap::new(),
            needs:             BTreeMap::new(),
            matrix:            false,
            matrix_total:      None,
            fail_fast:         true,
            max_parallel:      None,
            step_env:          BTreeMap::new(),
            action_inputs:     None,
            workflow_inputs:   None,
            secrets:           SecretMap::Inherit,
            callee_jobs:       None,
            in_expansion:      false,
            composite_outputs: BTreeMap::new(),
            start_node:        format!("{job_id}{SEP}start"),
        }
    }

    /// The run-context record for a node of this job, as an expression: static
    /// for a plain job, `nodes[name + '#' + index]` inside a matrix clone.
    pub(crate) fn node_record(&self, table: &mut ExprTable, name: &str) -> ExprId {
        let nodes = table.var("nodes");
        if self.matrix || self.in_expansion {
            let prefix = table.lit(format!("{name}#"));
            let index = table.var("index");
            let key = table.binary(BinOp::Add, prefix, index);
            table.index(nodes, key)
        } else {
            table.field(nodes, name)
        }
    }

    pub(crate) fn node_status(&self, table: &mut ExprTable, name: &str) -> ExprId {
        let record = self.node_record(table, name);
        table.field(record, "status")
    }

    /// `nodes[...].status == 'tag'`.
    pub(crate) fn node_has_status(&self, table: &mut ExprTable, name: &str, tag: &str) -> ExprId {
        let status = self.node_status(table, name);
        let lit = table.lit(tag);
        table.binary(BinOp::Eq, status, lit)
    }

    fn any_of(table: &mut ExprTable, terms: Vec<ExprId>) -> ExprId {
        let mut iter = terms.into_iter();
        let Some(first) = iter.next() else {
            return table.lit(false);
        };
        iter.fold(first, |acc, t| table.binary(BinOp::Or, acc, t))
    }

    /// `true` when some earlier step in this job ended in a real failure.
    pub(crate) fn earlier_step_failed(&self, table: &mut ExprTable) -> ExprId {
        let terms: Vec<ExprId> = self
            .earlier_steps
            .iter()
            .flat_map(|name| {
                [
                    self.node_has_status(table, name, "failure"),
                    self.node_has_status(table, name, "timed_out"),
                ]
            })
            .collect();
        Self::any_of(table, terms)
    }

    pub(crate) fn earlier_step_cancelled(&self, table: &mut ExprTable) -> ExprId {
        let terms: Vec<ExprId> = self
            .earlier_steps
            .iter()
            .map(|name| self.node_has_status(table, name, "cancelled"))
            .collect();
        Self::any_of(table, terms)
    }

    /// The job really started: its `start` node ran. A start skipped by the
    /// job's gate — or recorded `Cancelled` because the cancel landed before
    /// the job began — reads as not started, so the job's steps do not run.
    pub(crate) fn job_started(&self, table: &mut ExprTable) -> ExprId {
        self.node_has_status(table, &self.start_node, "success")
    }

    /// The job's `start` was cancelled before it could run, for the summary to
    /// report `cancelled` rather than `skipped`.
    pub(crate) fn start_cancelled(&self, table: &mut ExprTable) -> ExprId {
        self.node_has_status(table, &self.start_node, "cancelled")
    }

    /// The job's own gate admitted it *while the run was already cancelled* — a
    /// cleanup job (`if: always()` or `if: cancelled()`), whose interior steps
    /// then evaluate normally. The `start` noop records the engine's
    /// `scope_cancelled` static under `output.cancelled` when it fires.
    fn start_admitted_under_cancel(&self, table: &mut ExprTable) -> ExprId {
        let record = self.node_record(table, &self.start_node);
        let output = table.field(record, "output");
        let flag = table.field(output, "cancelled");
        let yes = table.lit(true);
        table.binary(BinOp::Eq, flag, yes)
    }

    /// The run was cancelled out from under this job: the scope is cancelled
    /// and the job was not admitted as cleanup. This is what makes a plain
    /// step read `success()` false after a cancel — GitHub's runner
    /// evaluates remaining steps with the job status `Cancelled` — while
    /// the steps of a cleanup job scheduled after the cancel evaluate
    /// normally.
    ///
    /// Composite inner steps skip the term: the caller's gate — evaluated at
    /// the caller's site, where the term applies — speaks for the
    /// interrupt, and a composite admitted by `always()` runs its inner
    /// steps as GitHub does.
    fn cancel_interrupt(&self, table: &mut ExprTable) -> ExprId {
        if self.action_inputs.is_some() {
            return table.lit(false);
        }
        let scoped = table.var("scope_cancelled");
        let admitted = self.start_admitted_under_cancel(table);
        let not_admitted = table.unary(UnOp::Not, admitted);
        table.binary(BinOp::And, scoped, not_admitted)
    }

    /// The record of a needed job's `done`. A matrix job's needs live *outside*
    /// its expansion region, so they read statically even when the job's own
    /// nodes take a `#index` suffix; inside an expanded call, sibling jobs are
    /// cloned together, so a needed done is suffixed like everything else.
    pub(crate) fn need_record(&self, table: &mut ExprTable, done_node: &str) -> ExprId {
        if self.in_expansion {
            self.node_record(table, done_node)
        } else {
            let nodes = table.var("nodes");
            table.field(nodes, done_node)
        }
    }

    /// A needed job's result: `nodes["N/done"].output.result`.
    pub(crate) fn need_result(&self, table: &mut ExprTable, done_node: &str) -> ExprId {
        let record = self.need_record(table, done_node);
        let output = table.field(record, "output");
        table.field(output, "result")
    }

    fn need_has_result(&self, table: &mut ExprTable, done_node: &str, tag: &str) -> ExprId {
        let result = self.need_result(table, done_node);
        let lit = table.lit(tag);
        table.binary(BinOp::Eq, result, lit)
    }

    /// Job-level `success()`: every needed job succeeded.
    pub(crate) fn needs_succeeded(&self, table: &mut ExprTable) -> ExprId {
        let mut acc = table.lit(true);
        for done in self.needs.values() {
            let ok = self.need_has_result(table, done, "success");
            acc = table.binary(BinOp::And, acc, ok);
        }
        acc
    }

    pub(crate) fn needs_failed(&self, table: &mut ExprTable) -> ExprId {
        let terms: Vec<ExprId> = self
            .needs
            .values()
            .map(|done| self.need_has_result(table, done, "failure"))
            .collect();
        Self::any_of(table, terms)
    }

    pub(crate) fn needs_cancelled(&self, table: &mut ExprTable) -> ExprId {
        let terms: Vec<ExprId> = self
            .needs
            .values()
            .map(|done| self.need_has_result(table, done, "cancelled"))
            .collect();
        Self::any_of(table, terms)
    }

    /// The status function `name()` at this site.
    ///
    /// In a step: over the earlier steps of the job (GitHub's job status). In a
    /// job's `if:`: over the needed jobs. `always()` is `true` everywhere.
    /// `cancelled()` also ORs in the engine's `scope_cancelled` static: a
    /// cancel that lands between steps cancels no step record, a
    /// not-yet-started job has no cancelled needs, and a `fail_fast` splice
    /// cancel is not a root cancel — `scope_cancelled` covers all three.
    /// `success()` is false under a cancel — a step's, unless the job was
    /// admitted as cleanup after the cancel ([`Self::cancel_interrupt`]); a
    /// job's, always — which is how a plain step or job stops when the run
    /// is cancelled without any admission flag sniffing condition text.
    pub(crate) fn status_function(
        &self,
        table: &mut ExprTable,
        name: &str,
        at: ExprSite,
    ) -> ExprId {
        match (name, at) {
            ("always", _) => table.lit(true),
            ("success", ExprSite::Step) => {
                let failed = self.earlier_step_failed(table);
                let cancelled = self.earlier_step_cancelled(table);
                let bad = table.binary(BinOp::Or, failed, cancelled);
                let interrupted = self.cancel_interrupt(table);
                let bad = table.binary(BinOp::Or, bad, interrupted);
                table.unary(UnOp::Not, bad)
            }
            ("failure", ExprSite::Step) => self.earlier_step_failed(table),
            ("success", ExprSite::Job) => {
                let ok = self.needs_succeeded(table);
                let scoped = table.var("scope_cancelled");
                let not_cancelled = table.unary(UnOp::Not, scoped);
                table.binary(BinOp::And, ok, not_cancelled)
            }
            ("failure", ExprSite::Job) => self.needs_failed(table),
            ("cancelled", _) => {
                let base = match at {
                    ExprSite::Step => self.earlier_step_cancelled(table),
                    ExprSite::Job => self.needs_cancelled(table),
                };
                let scoped = table.var("scope_cancelled");
                table.binary(BinOp::Or, base, scoped)
            }
            _ => table.lit(false),
        }
    }
}

/// How the lowering resolves GitHub's contexts.
pub(crate) struct GhaRoots<'s> {
    pub site:  &'s Site,
    pub at:    ExprSite,
    pub diags: &'s mut Diagnostics,
    pub span:  Span,
    /// The sentinel kinds this lowering produced; the caller decides which of
    /// them the position allows.
    pub saw:   SentinelSet,
}

/// Contexts that come from the run's parameters, looked up case-insensitively.
const PARAM_CONTEXTS: &[&str] = &["github", "vars", "runner"];

impl Roots for GhaRoots<'_> {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        let lowered = name.to_lowercase();
        match lowered.as_str() {
            n if PARAM_CONTEXTS.contains(&n) => Some(table.var(n)),
            "env" => Some(table.var("env")),
            "matrix" => {
                if self.site.matrix {
                    Some(table.var("item"))
                } else {
                    Some(table.lit(Value::Null))
                }
            }
            "secrets" => {
                self.saw.insert(Sentinel::Secret);
                // Lowered to a marker the caller checks for; it never evaluates.
                Some(table.lit(Value::Null))
            }
            "inputs" => {
                if self.site.action_inputs.is_none() && self.site.workflow_inputs.is_none() {
                    // No composite and no workflow_call/dispatch declaration in
                    // scope: GitHub evaluates the context as empty, and so does
                    // this run — loudly, since it is almost always a mistake.
                    self.diags.warning(
                        "gha.inputs_undeclared",
                        self.span.clone(),
                        "the `inputs` context is empty here: this workflow declares no \
                         `workflow_call` or `workflow_dispatch` inputs",
                    );
                }
                Some(table.lit(Value::Null))
            }
            "steps" | "needs" | "job" | "strategy" => Some(table.lit(Value::Null)),
            "jobs" => {
                if self.site.callee_jobs.is_none() {
                    self.diags.error(
                        "gha.jobs_context",
                        self.span.clone(),
                        "the `jobs` context exists only in `on.workflow_call.outputs` values",
                    );
                }
                Some(table.lit(Value::Null))
            }
            _ => None,
        }
    }

    fn path(&mut self, root: &str, path: &[&str], table: &mut ExprTable) -> Option<ExprId> {
        match root.to_lowercase().as_str() {
            "steps" => {
                let [id, what, rest @ ..] = path else {
                    return Some(table.lit(Value::Null));
                };
                let Some(node) = self.site.step_names.get(*id) else {
                    self.diags.warning(
                        "gha.unknown_step",
                        self.span.clone(),
                        format!(
                            "`steps.{id}` does not name a step in job `{}`; it evaluates to null",
                            self.site.job_id
                        ),
                    );
                    return Some(table.lit(Value::Null));
                };
                let node = node.clone();
                match what.to_lowercase().as_str() {
                    "outputs" => {
                        // A composite step's outputs are expressions the action declared.
                        if let Some(outputs) = self.site.composite_outputs.get(*id) {
                            let Some(name) = rest.first() else {
                                return Some(table.lit(Value::Null));
                            };
                            return Some(match outputs.get(*name) {
                                Some(id) => *id,
                                None => table.lit(Value::Null),
                            });
                        }
                        let record = self.site.node_record(table, &node);
                        let mut id = table.field(record, "output");
                        for key in rest {
                            id = table.field(id, key);
                        }
                        Some(id)
                    }
                    "outcome" => {
                        let status = self.site.node_status(table, &node);
                        Some(outcome_tag(table, status))
                    }
                    "conclusion" => {
                        let status = self.site.node_status(table, &node);
                        Some(conclusion_tag(table, status))
                    }
                    _ => Some(table.lit(Value::Null)),
                }
            }
            "needs" => {
                let [job, what, rest @ ..] = path else {
                    return Some(table.lit(Value::Null));
                };
                let Some(done) = self.site.needs.get(*job) else {
                    self.diags.error(
                        "gha.needs_unknown",
                        self.span.clone(),
                        format!(
                            "`needs.{job}` is not in job `{}`'s `needs`",
                            self.site.job_id
                        ),
                    );
                    return Some(table.lit(Value::Null));
                };
                let done = done.clone();
                Some(done_projection(self.site, table, &done, what, rest))
            }
            "job" => match path.first().map(|s| s.to_lowercase()).as_deref() {
                Some("status") => {
                    let failed = self.site.earlier_step_failed(table);
                    let cancelled = self.site.earlier_step_cancelled(table);
                    let interrupted = self.site.cancel_interrupt(table);
                    let cancelled = table.binary(BinOp::Or, cancelled, interrupted);
                    let f = table.lit("failure");
                    let c = table.lit("cancelled");
                    let s = table.lit("success");
                    let inner = table.cond(cancelled, c, s);
                    Some(table.cond(failed, f, inner))
                }
                Some("container" | "services") => {
                    self.diags.unsupported(
                        "job_context",
                        self.span.clone(),
                        format!("`job.{}` reads runtime container details", path[0]),
                        "container ids, networks and port mappings are not exposed to \
                         expressions yet; reach a service by its name and declared ports",
                    );
                    None
                }
                _ => Some(table.lit(Value::Null)),
            },
            "strategy" => match path.first().map(|s| s.to_lowercase()).as_deref() {
                Some("fail-fast") => Some(table.lit(self.site.fail_fast)),
                Some("max-parallel") => Some(match self.site.max_parallel {
                    Some(n) => table.lit(n),
                    None => table.lit(Value::Null),
                }),
                Some("job-index") => Some(if self.site.matrix {
                    table.var("index")
                } else {
                    table.lit(0)
                }),
                Some("job-total") => match (self.site.matrix, self.site.matrix_total) {
                    (false, _) => Some(table.lit(1)),
                    (true, Some(n)) => Some(table.lit(n as u64)),
                    (true, None) => {
                        self.diags.unsupported(
                            "strategy.job_total.dynamic",
                            self.span.clone(),
                            "`strategy.job-total` on an expression-valued matrix",
                            "the leg count is only known at run time; use a static matrix",
                        );
                        None
                    }
                },
                _ => Some(table.lit(Value::Null)),
            },
            "matrix" => {
                if !self.site.matrix {
                    return Some(table.lit(Value::Null));
                }
                let mut id = table.var("item");
                for key in path {
                    let k = table.lit(*key);
                    id = builtin(table, "get_ci", vec![id, k]).ok()?;
                }
                Some(id)
            }
            "inputs" => {
                // A composite body's own inputs shadow the workflow's.
                let inputs = self
                    .site
                    .action_inputs
                    .as_ref()
                    .or(self.site.workflow_inputs.as_ref())?;
                let Some(name) = path.first() else {
                    return Some(table.lit(Value::Null));
                };
                let lowered = name.to_lowercase();
                let found = inputs
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == lowered)
                    .map(|(_, v)| *v);
                Some(found.unwrap_or_else(|| table.lit(Value::Null)))
            }
            // Inside `on.workflow_call.outputs` values: `jobs.<id>.outputs.*` and
            // `jobs.<id>.result` read the (inlined) job's done record, exactly as
            // `needs.*` does.
            "jobs" if self.site.callee_jobs.is_some() => {
                let callee_jobs = self
                    .site
                    .callee_jobs
                    .as_ref()
                    .expect("the match guard proved the site has callee jobs");
                let [job, what, rest @ ..] = path else {
                    return Some(table.lit(Value::Null));
                };
                let Some(done) = callee_jobs.get(*job) else {
                    self.diags.error(
                        "gha.jobs_unknown",
                        self.span.clone(),
                        format!("`jobs.{job}` does not name a job of this workflow"),
                    );
                    return Some(table.lit(Value::Null));
                };
                let done = done.clone();
                Some(done_projection(self.site, table, &done, what, rest))
            }
            "env" => {
                // A step's own env is visible to its expressions; otherwise the
                // scope's env.
                let Some(name) = path.first() else {
                    return Some(table.var("env"));
                };
                if path.len() == 1
                    && let Some(value) = self.site.step_env.get(*name)
                {
                    return Some(match value {
                        ExprOrValue::Value(v) => table.lit(v.clone()),
                        ExprOrValue::Expr(id) => *id,
                    });
                }
                None
            }
            // A secret inside an expression lowers to its sentinel: the expression
            // evaluates in the engine over the sentinel, never the value, and the
            // step splices the value in at spawn. The caller decides whether this
            // position may carry one at all. Inside a called workflow the name
            // maps through the call's `secrets:` first — a pure rename.
            "secrets" => {
                self.saw.insert(Sentinel::Secret);
                Some(match path {
                    [name] => match self.site.secrets.resolve(name) {
                        Ok(Some(provider)) => table.lit(Sentinel::secret(&provider)),
                        // Declared but not provided: the value is empty, as on GitHub.
                        Ok(None) => table.lit(""),
                        Err(UndeclaredSecret) => {
                            undeclared_secret(self.diags, self.span.clone(), name);
                            table.lit("")
                        }
                    },
                    _ => table.lit(Value::Null),
                })
            }
            // `github.workspace` is runner-side truth: only the step's
            // environment knows the path, so in step positions it lowers to a
            // sentinel the step kinds substitute at spawn
            // ([`Sentinel::WORKSPACE_MARKER`]). A scope position (a job-level `env:`)
            // is resolved at acquire, where nothing could substitute — the
            // reference stays a parameter read there (null, rendered empty).
            "github"
                if self.at == ExprSite::Step
                    && matches!(path, [key] if key.eq_ignore_ascii_case("workspace")) =>
            {
                self.saw.insert(Sentinel::Workspace);
                Some(table.lit(Sentinel::WORKSPACE_MARKER))
            }
            // `runner.temp` is runner-side truth the same way: `RUNNER_TEMP`
            // is set by the step's environment (a host path, or a container
            // mount), so in step positions it lowers to a sentinel the step
            // kinds substitute at spawn ([`Sentinel::RUNNER_TEMP_MARKER`]). A scope
            // position stays a parameter read — null, rendered empty — as
            // `github.workspace` does. `runner.os/arch/name` stay parameters.
            "runner"
                if self.at == ExprSite::Step
                    && matches!(path, [key] if key.eq_ignore_ascii_case("temp")) =>
            {
                self.saw.insert(Sentinel::RunnerTemp);
                Some(table.lit(Sentinel::RUNNER_TEMP_MARKER))
            }
            // `runner.tool_cache` follows: the step computes the one resolved
            // value (its own env wins, else the environment's ambient
            // `RUNNER_TOOL_CACHE`, else the host store where this filesystem
            // has it, else the workspace directory) and substitutes the
            // sentinel with exactly what it exports as the variable
            // ([`Sentinel::RUNNER_TOOL_CACHE_MARKER`]). `runner.os/arch/name` stay
            // parameters.
            "runner"
                if self.at == ExprSite::Step
                    && matches!(path, [key] if key.eq_ignore_ascii_case("tool_cache")) =>
            {
                self.saw.insert(Sentinel::RunnerToolCache);
                Some(table.lit(Sentinel::RUNNER_TOOL_CACHE_MARKER))
            }
            // `github.token` is a secret, not a parameter: the same rule as `secrets.*`.
            "github" if matches!(path, [token] if token.eq_ignore_ascii_case("token")) => {
                self.saw.insert(Sentinel::Secret);
                Some(table.lit(Sentinel::secret(GITHUB_TOKEN_SECRET)))
            }
            _ => None,
        }
    }

    fn call(
        &mut self,
        name: &str,
        args: &[Expr],
        table: &mut ExprTable,
    ) -> Option<Result<ExprId, LowerError>> {
        match name.to_lowercase().as_str() {
            n @ ("always" | "success" | "failure" | "cancelled") => {
                if !args.is_empty() {
                    return Some(Err(LowerError::Arity {
                        name:     name.to_string(),
                        expected: 0,
                        got:      args.len(),
                    }));
                }
                Some(Ok(self.site.status_function(table, n, self.at)))
            }
            "hashfiles" => {
                // Literal patterns lower to a sentinel the step resolves against
                // the workspace at spawn — the expression evaluates over the
                // sentinel, never the hash. The caller decides whether this
                // position may carry one (`run:`, `env:`, `with:`) at all.
                match literal_hashfiles_patterns(args, &self.span, self.diags) {
                    Some(patterns) => {
                        self.saw.insert(Sentinel::HashFiles);
                        Some(Ok(table.lit(Sentinel::hashfiles(&patterns))))
                    }
                    None => Some(Err(LowerError::Custom("hashFiles is not supported".into()))),
                }
            }
            _ => None,
        }
    }
}

/// `<done>.result` or `<done>.outputs.…` off a job's done record — the shared
/// projection behind the `needs.*` and `jobs.*` contexts.
fn done_projection(
    site: &Site,
    table: &mut ExprTable,
    done: &str,
    what: &str,
    rest: &[&str],
) -> ExprId {
    match what.to_lowercase().as_str() {
        "result" => site.need_result(table, done),
        "outputs" => {
            let record = site.need_record(table, done);
            let output = table.field(record, "output");
            let mut id = table.field(output, "outputs");
            for key in rest {
                id = table.field(id, key);
            }
            id
        }
        _ => table.lit(Value::Null),
    }
}

/// The error for a `secrets.*` name this workflow call never granted.
pub(crate) fn undeclared_secret(diags: &mut Diagnostics, span: Span, name: &str) {
    diags.error(
        "gha.undeclared_secret",
        span,
        format!(
            "`secrets.{name}` is not a secret this workflow call provides; declare it \
             under `on.workflow_call.secrets` and pass it (or use `secrets: inherit`)"
        ),
    );
}

/// GitHub's combined-conclusion rule over a set of results: any failure wins,
/// then any cancellation, then any success; nothing at all is `skipped`.
pub(crate) fn result_priority(
    table: &mut ExprTable,
    any_failure: ExprId,
    any_cancelled: ExprId,
    any_success: ExprId,
) -> ExprId {
    let failure = table.lit("failure");
    let cancelled = table.lit("cancelled");
    let success = table.lit("success");
    let skipped = table.lit("skipped");
    let inner2 = table.cond(any_success, success, skipped);
    let inner1 = table.cond(any_cancelled, cancelled, inner2);
    table.cond(any_failure, failure, inner1)
}

/// The patterns of a `hashFiles(...)` call when every argument is a literal
/// string (and there is at least one). Anything else — computed patterns, no
/// patterns — gets the one shared diagnostic: the rule and its wording live
/// here, for every position that lowers the call to a sentinel.
pub(crate) fn literal_hashfiles_patterns(
    args: &[Expr],
    span: &Span,
    diags: &mut Diagnostics,
) -> Option<Vec<String>> {
    let patterns: Option<Vec<String>> = args
        .iter()
        .map(|arg| match arg {
            Expr::Literal(Literal::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let Some(patterns) = patterns.filter(|p| !p.is_empty()) else {
        diags.unsupported(
            "expression.hashFiles",
            span.clone(),
            "`hashFiles()` with computed patterns",
            "the step resolves `hashFiles` against the workspace at spawn, so its \
             patterns must be literal strings in the workflow",
        );
        return None;
    };
    Some(patterns)
}

/// One parsed expression lowered through the GHA roots, with the sentinel
/// kinds a caller needs for its position-specific rules.
pub(crate) struct LoweredExpr {
    pub id:  ExprId,
    pub saw: SentinelSet,
}

/// Lower one parsed expression through [`GhaRoots`], mapping lowering failures
/// to their diagnostics. `None` means a diagnostic was reported.
pub(crate) fn lower_expr(
    ast: &Expr,
    site: &Site,
    at: ExprSite,
    span: &Span,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<LoweredExpr> {
    let mut roots = GhaRoots {
        site,
        at,
        diags,
        span: span.clone(),
        saw: SentinelSet::default(),
    };
    let id = match gha(ast, table, &mut roots) {
        Ok(id) => id,
        Err(LowerError::UnknownIdent(name)) => {
            roots.diags.error(
                "expr.unknown_context",
                span.clone(),
                format!("`{name}` is not a GitHub Actions context"),
            );
            return None;
        }
        Err(LowerError::Custom(_)) => return None,
        Err(e) => {
            roots.diags.error("expr.lower", span.clone(), e.to_string());
            return None;
        }
    };
    Some(LoweredExpr { id, saw: roots.saw })
}

/// The one shape a GHA string template takes as an expression: a lone
/// `${{ … }}` keeps its value; a mixed template concatenates its pieces, each
/// expression stringified through `loose_string`. Callers differ only in how a
/// literal piece becomes text (`text_value`), how one expression source lowers
/// (`lower_one`), and what a builtin failure maps to (`bad`) — the fold itself
/// is written once, so template semantics cannot drift between positions.
pub(crate) fn fold_template<E>(
    segments: &[Segment],
    table: &mut ExprTable,
    mut text_value: impl FnMut(&str) -> String,
    mut lower_one: impl FnMut(&str, &mut ExprTable) -> Result<ExprId, E>,
    mut bad: impl FnMut(String) -> E,
) -> Result<ExprId, E> {
    if let [Segment::Expr { source, .. }] = segments {
        return lower_one(source, table);
    }
    let mut pieces = Vec::with_capacity(segments.len());
    for segment in segments {
        match segment {
            Segment::Text(t) => pieces.push(table.lit(text_value(t))),
            Segment::Expr { source, .. } => {
                let id = lower_one(source, table)?;
                let as_string =
                    builtin(table, "loose_string", vec![id]).map_err(|e| bad(e.to_string()))?;
                pieces.push(as_string);
            }
        }
    }
    let mut iter = pieces.into_iter();
    let mut acc = iter.next().expect("a template has at least one segment");
    for next in iter {
        acc = table.binary(BinOp::Concat, acc, next);
    }
    Ok(acc)
}

/// What [`lower_scalar`] made of one workflow scalar. `Secret` appears only at
/// `env_shaped` sites — a step's `run:`, `env:` or `with:`; anywhere else a
/// whole-value secret is a diagnostic instead.
pub(crate) enum LoweredScalar {
    /// A plain value, no expression in it.
    Literal(Value),
    /// An expression.
    Expr(ExprId),
    /// The whole value was `${{ secrets.NAME }}`.
    Secret(String),
}

/// Lower one scalar from the workflow: literal, templated string, or a whole
/// expression.
///
/// `env_shaped` says whether secrets may appear here at all: in a step's config
/// — `run:`, `env:`, `with:` — they may, because the step resolves them at
/// spawn. A bare `${{ secrets.X }}` becomes a `$secret` reference; a secret
/// inside a larger expression or string becomes its sentinel in the lowered
/// text. Anywhere else — an `if:`, a job output, a matrix — a secret is
/// rejected: the engine would be evaluating over the sentinel and calling it
/// the value.
pub(crate) fn lower_scalar(
    text: &str,
    span: Span,
    site: &Site,
    at: ExprSite,
    env_shaped: bool,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<LoweredScalar> {
    if !text.contains("${{") {
        let text = if env_shaped {
            escape_sentinel_text(text)
        } else {
            text.to_string()
        };
        return Some(LoweredScalar::Literal(Value::String(text)));
    }
    let Ok(segments) = split_template(text) else {
        diags.error("expr.unterminated", span, "unterminated `${{`");
        return None;
    };

    // A whole-value secret reference is the one permitted form. The name maps
    // through the site's call `secrets:` first — a pure rename.
    if let [Segment::Expr { source, .. }] = segments.as_slice()
        && let Some(name) = secret_expr_name(source)
    {
        if env_shaped {
            return Some(match site.secrets.resolve(&name) {
                Ok(Some(provider)) => LoweredScalar::Secret(provider),
                // Declared but not provided: the value is empty, as on GitHub.
                Ok(None) => LoweredScalar::Literal(Value::String(String::new())),
                Err(UndeclaredSecret) => {
                    undeclared_secret(diags, span, &name);
                    return None;
                }
            });
        }
        diags.unsupported(
            "secrets.expression",
            span,
            format!(
                "`{}` used as an expression rather than as the whole value of an `env:` or `with:` entry",
                source.trim()
            ),
            "secrets are absent from the expression environment by construction, so they never reach the \
             event log; pass the secret through an environment variable and test it in the step",
        );
        return None;
    }

    let acc = fold_template(
        &segments,
        table,
        |t| {
            if env_shaped {
                escape_sentinel_text(t)
            } else {
                t.to_string()
            }
        },
        |source, table| {
            let ast = match parse(source) {
                Ok(ast) => ast,
                Err(e) => {
                    diags.error(
                        "expr.parse",
                        span.clone(),
                        format!("could not parse `${{{{ {} }}}}`: {e}", source.trim()),
                    );
                    return Err(());
                }
            };
            // A bare `env.NAME` alone in its segment becomes the env sentinel:
            // the step substitutes it at spawn from the environment its
            // process receives, so `GITHUB_ENV` appends from earlier steps
            // are visible — the gate's `$env` leaf, for config text. Under an
            // operator or function the engine would evaluate over the marker,
            // so those fall through and read the scope env, as conditions do.
            if env_shaped
                && at == ExprSite::Step
                && let Some(name) = bare_env_name(&ast)
            {
                return Ok(table.lit(Sentinel::env(&name)));
            }
            let lowered = lower_expr(&ast, site, at, &span, table, diags).ok_or(())?;
            if lowered.saw.contains(Sentinel::Secret) && !env_shaped {
                diags.unsupported(
                    "secrets.expression",
                    span.clone(),
                    "a `secrets.*` or `github.token` reference in a position the engine evaluates",
                    "secrets are absent from the expression environment by construction, so they never reach the \
                     event log; a secret may appear in a step's `run:`, `env:` or `with:`, where the step \
                     resolves it, but not in an `if:`, an output or a matrix",
                );
                return Err(());
            }
            if lowered.saw.contains(Sentinel::HashFiles) && !env_shaped {
                diags.unsupported(
                    "expression.hashFiles",
                    span.clone(),
                    "a `hashFiles()` call in a position the engine evaluates",
                    "the step resolves `hashFiles` against the workspace at spawn, so it may \
                     appear in a step's `run:`, `env:` or `with:`, but not in an `if:`, an \
                     output or a matrix",
                );
                return Err(());
            }
            Ok(lowered.id)
        },
        |_| (),
    )
    .ok()?;
    Some(LoweredScalar::Expr(acc))
}

/// The secret `text` names when it is exactly one `${{ secrets.X }}` (or
/// `${{ github.token }}`) reference and nothing else. For the positions that
/// drop such a value with a warning — a job output — rather than rejecting it.
pub(crate) fn whole_value_secret(text: &str) -> Option<String> {
    match split_template(text).ok()?.as_slice() {
        [Segment::Expr { source, .. }] => secret_expr_name(source),
        _ => None,
    }
}

/// The variable `expr` reads when it is exactly `env.NAME` (or `env['NAME']`)
/// and the name cannot impersonate a marker — the shape [`lower_scalar`] turns
/// into an env sentinel in step config.
fn bare_env_name(expr: &Expr) -> Option<String> {
    let (root, path) = expr.dotted_path()?;
    if !root.eq_ignore_ascii_case("env") {
        return None;
    }
    match path.as_slice() {
        [name] if !name.chars().any(|c| ('\u{E000}'..='\u{E002}').contains(&c)) => {
            Some(name.to_string())
        }
        _ => None,
    }
}

/// The secret one expression source names when it is exactly `secrets.X` or
/// `github.token` — the shared definition behind [`whole_value_secret`] and
/// [`lower_scalar`]'s whole-value check.
fn secret_expr_name(source: &str) -> Option<String> {
    let ast = parse(source).ok()?;
    let (root, path) = ast.dotted_path()?;
    secret_name(root, &path)
}

/// The secret a whole-value reference names: `secrets.X` is `X`, and
/// `github.token` is [`GITHUB_TOKEN_SECRET`] — the token is a secret the run's
/// provider holds, never a run parameter, so it cannot reach the graph or the
/// log.
fn secret_name(root: &str, path: &[&str]) -> Option<String> {
    match (root.to_ascii_lowercase().as_str(), path) {
        ("secrets", [name]) => Some(name.to_string()),
        ("github", [token]) if token.eq_ignore_ascii_case("token") => {
            Some(GITHUB_TOKEN_SECRET.to_string())
        }
        _ => None,
    }
}

/// The secret name `github.token` resolves to.
pub const GITHUB_TOKEN_SECRET: &str = "GITHUB_TOKEN";

const SENTINEL_CLOSE: &str = "\u{E001}";
const SENTINEL_ESCAPE: char = '\u{E002}';

/// One kind of sentinel: a stand-in the lowering writes into a string where
/// GitHub renders a value only the step can know. The graph, the resolved
/// config and the event log carry the marker in the value's place; the step
/// kinds that run GitHub steps substitute the value at spawn, so the
/// expression and the environment cannot diverge. Private-use characters
/// bracket every marker, and literal workflow text escapes those characters
/// first ([`escape_sentinel_text`]), so user text cannot forge one.
///
/// Three kinds carry a payload between the brackets — a name, or a pattern
/// list — and three are constant markers for runner-side paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sentinel {
    /// `secrets.NAME` (and `github.token`): the marker carries the provider's
    /// secret name ([`Self::secret`]). The expression evaluates over the
    /// marker, never the value.
    Secret,
    /// A bare `${{ env.NAME }}` in step config (`run:`, `env:`, `with:`),
    /// carrying the variable name as [`Self::Secret`] carries its. The
    /// engine's `env` binding is the scope env, frozen at firing, so it can
    /// never see what an earlier step appended through `GITHUB_ENV`; the step
    /// substitutes this marker at spawn from the environment its process
    /// receives — the gate's `$env` leaf, in flat-text form. Only a bare
    /// reference standing alone in its template segment lowers this way
    /// ([`lower_scalar`]): under an operator or function the engine would
    /// evaluate over the marker, so those keep the engine's scope-env
    /// meaning, exactly as they do in a condition.
    Env,
    /// `hashFiles(patterns…)`: the marker carries the pattern list
    /// ([`Self::hashfiles`]). The step kinds replace it with the hash at
    /// spawn ([`Self::resolve_hashfiles`]), computed against the workspace.
    HashFiles,
    /// `github.workspace`: the constant marker [`Self::WORKSPACE_MARKER`].
    /// GitHub renders the context to the runner-side workspace path, which
    /// only the step's environment knows — a host path, or the container's
    /// mount point — so the step kinds substitute their own
    /// `GITHUB_WORKSPACE` at spawn.
    Workspace,
    /// `runner.temp`: the constant marker [`Self::RUNNER_TEMP_MARKER`], for
    /// the same reason as [`Self::Workspace`] — only the step's environment
    /// knows the path `RUNNER_TEMP` carries, so the step kinds substitute
    /// exactly that value at spawn, and the two cannot diverge.
    RunnerTemp,
    /// `runner.tool_cache`: the constant marker
    /// [`Self::RUNNER_TOOL_CACHE_MARKER`]. The value is not a static path —
    /// an image's own populated cache wins, else the host store where this
    /// filesystem has it, else the workspace directory — and the step kinds
    /// substitute exactly the value they export as `RUNNER_TOOL_CACHE`, so
    /// the expression and the environment cannot diverge.
    RunnerToolCache,
}

impl Sentinel {
    /// Every kind, in the order the gate diagnostics report them.
    pub(crate) const ALL: [Self; 6] = [
        Self::Secret,
        Self::Env,
        Self::HashFiles,
        Self::Workspace,
        Self::RunnerTemp,
        Self::RunnerToolCache,
    ];
    /// The whole marker for `runner.temp`.
    pub const RUNNER_TEMP_MARKER: &'static str = "\u{E000}petri-runner-temp\u{E001}";
    /// The whole marker for `runner.tool_cache`.
    pub const RUNNER_TOOL_CACHE_MARKER: &'static str = "\u{E000}petri-runner-tool-cache\u{E001}";
    /// The whole marker for `github.workspace`.
    pub const WORKSPACE_MARKER: &'static str = "\u{E000}petri-workspace\u{E001}";

    /// The text every marker of this kind starts with.
    const fn open(self) -> &'static str {
        match self {
            Self::Secret => "\u{E000}petri-secret:",
            Self::Env => "\u{E000}petri-env:",
            Self::HashFiles => "\u{E000}petri-hashfiles:",
            Self::Workspace => "\u{E000}petri-workspace",
            Self::RunnerTemp => "\u{E000}petri-runner-temp",
            Self::RunnerToolCache => "\u{E000}petri-runner-tool-cache",
        }
    }

    /// What [`Self::present_in`] scans for: the open prefix of a payload
    /// kind, the whole marker of a constant kind. The distinction is
    /// load-bearing: a hashFiles payload is workflow-author text that may
    /// spell out another kind's open prefix, but the close character can
    /// never appear inside a payload ([`Self::hashfiles`] escapes it away),
    /// so a whole marker cannot be forged.
    const fn needle(self) -> &'static str {
        match self {
            Self::Secret | Self::Env | Self::HashFiles => self.open(),
            Self::Workspace => Self::WORKSPACE_MARKER,
            Self::RunnerTemp => Self::RUNNER_TEMP_MARKER,
            Self::RunnerToolCache => Self::RUNNER_TOOL_CACHE_MARKER,
        }
    }

    /// The marker for secret `name`: what a lowered string carries in the
    /// value's place until a step kind resolves it at spawn
    /// ([`Self::resolve_in`]).
    pub fn secret(name: &str) -> String {
        format!("{}{name}{SENTINEL_CLOSE}", Self::Secret.open())
    }

    /// The marker for a bare `env.NAME` reference in step config.
    pub fn env(name: &str) -> String {
        format!("{}{name}{SENTINEL_CLOSE}", Self::Env.open())
    }

    /// The marker for `hashFiles(patterns…)`, carrying the pattern list as
    /// JSON with the close character escaped out of the payload.
    pub fn hashfiles(patterns: &[String]) -> String {
        let payload = serde_json::to_string(patterns)
            .expect("strings encode")
            .replace(SENTINEL_CLOSE, "\\uE001");
        format!("{}{payload}{SENTINEL_CLOSE}", Self::HashFiles.open())
    }

    /// Whether `text` carries a marker of this kind.
    pub fn present_in(self, text: &str) -> bool {
        text.contains(self.needle())
    }

    /// Every marker of this kind replaced by `value` — the consuming side of
    /// the constant kinds, whose one runner-side value the caller has
    /// resolved. `None` means no marker is present and `text` stands as it
    /// is, the shape the config-rewriting passes expect. The payload kinds
    /// resolve per payload instead ([`Self::resolve_in`],
    /// [`Self::resolve_hashfiles`]).
    pub fn replace_in(self, text: &str, value: &str) -> Option<String> {
        if !self.present_in(text) {
            return None;
        }
        match self {
            Self::Workspace | Self::RunnerTemp | Self::RunnerToolCache => {
                Some(text.replace(self.needle(), value))
            }
            Self::Secret | Self::Env | Self::HashFiles => {
                let replaced: Result<String, Infallible> =
                    self.resolve_in(text, |_| Ok(value.to_owned()));
                Some(replaced.expect("the resolver is infallible"))
            }
        }
    }

    /// Replace every marker of a name-carrying kind — [`Self::Secret`] or
    /// [`Self::Env`] — with what `resolve` returns for its name.
    pub fn resolve_in<E>(
        self,
        text: &str,
        mut resolve: impl FnMut(&str) -> Result<String, E>,
    ) -> Result<String, E> {
        replace_marked(text, self.open(), &mut resolve)
    }

    /// Every pattern list named by a hashFiles marker in `text`, in order of
    /// appearance. A run-time caller computes each hash asynchronously, then
    /// splices the results in with [`Self::resolve_hashfiles`].
    pub fn hashfiles_calls(text: &str) -> Vec<Vec<String>> {
        let open = Self::HashFiles.open();
        if !Self::HashFiles.present_in(text) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find(open) {
            let after = &rest[start + open.len()..];
            match after.find(SENTINEL_CLOSE) {
                Some(end) => {
                    let payload = &after[..end];
                    if let Ok(patterns) = serde_json::from_str::<Vec<String>>(payload) {
                        out.push(patterns);
                    }
                    rest = &after[end + SENTINEL_CLOSE.len()..];
                }
                None => break,
            }
        }
        out
    }

    /// Replace every hashFiles marker in `text` with what `resolve` returns
    /// for its pattern list.
    pub fn resolve_hashfiles<E>(
        text: &str,
        mut resolve: impl FnMut(&[String]) -> Result<String, E>,
    ) -> Result<String, E> {
        let open = Self::HashFiles.open();
        replace_marked(text, open, &mut |payload| {
            match serde_json::from_str::<Vec<String>>(payload) {
                Ok(patterns) => resolve(&patterns),
                // Not a payload this crate wrote; keep it as text.
                Err(_) => Ok(format!("{open}{payload}{SENTINEL_CLOSE}")),
            }
        })
    }

    /// The rule and wording for a kind that may not appear in a gate
    /// condition where the engine would evaluate over the raw marker; `None`
    /// for [`Self::Env`], which conditions carry as `$env` leaves.
    pub(crate) fn gate_diagnostic(self) -> Option<GateDiagnostic> {
        match self {
            Self::Secret => Some(GateDiagnostic {
                code:    "secrets.expression",
                subject: "a `secrets.*` or `github.token` reference in a condition",
                hint:    "secrets are absent from the expression environment by construction, so they never reach the \
                          event log; pass the secret through an environment variable and test it in the step \
                          (`env.NAME`), but a condition cannot read the secret itself",
            }),
            Self::Env => None,
            Self::HashFiles => Some(GateDiagnostic {
                code:    "expression.hashFiles",
                subject: "a `hashFiles()` call under a function the engine evaluates",
                hint:    "in a condition, `hashFiles(...)` may stand alone or under the comparison and boolean \
                          operators, where the step resolves it; under other functions the engine would \
                          evaluate over the unresolved sentinel",
            }),
            Self::Workspace => Some(GateDiagnostic {
                code:    "expression.workspace",
                subject: "`github.workspace` under a function the engine evaluates",
                hint:    "the workspace path is known only to the step's environment; in a condition, \
                          `github.workspace` may stand alone or under the comparison and boolean operators, \
                          where the step resolves it — or read `GITHUB_WORKSPACE` in the step itself",
            }),
            Self::RunnerTemp => Some(GateDiagnostic {
                code:    "expression.runner_temp",
                subject: "`runner.temp` under a function the engine evaluates",
                hint:    "the temp path is known only to the step's environment; in a condition, \
                          `runner.temp` may stand alone or under the comparison and boolean operators, \
                          where the step resolves it — or read `RUNNER_TEMP` in the step itself",
            }),
            Self::RunnerToolCache => Some(GateDiagnostic {
                code:    "expression.runner_tool_cache",
                subject: "`runner.tool_cache` under a function the engine evaluates",
                hint:    "the tool cache path is known only to the step's environment; in a condition, \
                          `runner.tool_cache` may stand alone or under the comparison and boolean operators, \
                          where the step resolves it — or read `RUNNER_TOOL_CACHE` in the step itself",
            }),
        }
    }
}

/// The diagnostic for a sentinel kind a gate condition may not carry under a
/// function the engine evaluates: its `unsupported` code, the subject phrase,
/// and the hint.
#[derive(Clone, Copy)]
pub(crate) struct GateDiagnostic {
    pub code:    &'static str,
    pub subject: &'static str,
    pub hint:    &'static str,
}

/// The sentinel kinds one lowering produced, for the caller's
/// position-specific rules: a position that cannot carry a kind turns its
/// presence into a diagnostic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SentinelSet(u8);

impl SentinelSet {
    pub(crate) fn insert(&mut self, kind: Sentinel) {
        self.0 |= Self::bit(kind);
    }

    pub(crate) fn contains(self, kind: Sentinel) -> bool {
        self.0 & Self::bit(kind) != 0
    }

    const fn bit(kind: Sentinel) -> u8 {
        1 << kind as u8
    }
}

/// Escape private-use marker characters in literal workflow text. Generated
/// placeholders are added after this step, so literal text cannot impersonate
/// one — the forgery invariant every [`Sentinel`] kind rests on.
pub fn escape_sentinel_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\u{E000}' => out.push_str("\u{E002}0"),
            '\u{E001}' => out.push_str("\u{E002}1"),
            '\u{E002}' => out.push_str("\u{E002}2"),
            other => out.push(other),
        }
    }
    out
}

pub fn has_sentinel_escape(text: &str) -> bool {
    text.contains(SENTINEL_ESCAPE)
}

/// Restore text escaped by [`escape_sentinel_text`] after generated
/// placeholders have been resolved.
pub fn unescape_sentinel_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != SENTINEL_ESCAPE {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('0') => out.push('\u{E000}'),
            Some('1') => out.push('\u{E001}'),
            // An escaped escape, and a text that ends mid-escape: both keep the
            // one character.
            Some('2') | None => out.push(SENTINEL_ESCAPE),
            Some(other) => {
                out.push(SENTINEL_ESCAPE);
                out.push(other);
            }
        }
    }
    out
}

/// Replace every `<open>payload\u{E001}` marker in `text` with what `resolve`
/// returns for its payload.
fn replace_marked<E>(
    text: &str,
    open: &str,
    resolve: &mut dyn FnMut(&str) -> Result<String, E>,
) -> Result<String, E> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        if let Some(end) = after.find(SENTINEL_CLOSE) {
            out.push_str(&resolve(&after[..end])?);
            rest = &after[end + SENTINEL_CLOSE.len()..];
        } else {
            // An opener with no closer is not ours; keep it as text.
            out.push_str(&rest[start..start + open.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A config value from a lowered scalar: literal, `{"$expr": id}`, or
/// `{"$secret": name}`.
pub(crate) fn config_value(lowered: LoweredScalar) -> Value {
    match lowered {
        LoweredScalar::Literal(v) => v,
        LoweredScalar::Expr(id) => json!({ EXPR_PLACEHOLDER_KEY: id.raw() }),
        LoweredScalar::Secret(name) => json!({ SECRET_REF_KEY: name }),
    }
}

#[cfg(test)]
mod sentinel_tests {
    use super::*;

    #[test]
    fn sentinels_round_trip_through_replacement() {
        let text = format!(
            "token {} and {}!",
            Sentinel::secret("GITHUB_TOKEN"),
            Sentinel::secret("OTHER")
        );
        assert!(Sentinel::Secret.present_in(&text));
        let out = Sentinel::Secret
            .resolve_in(&text, |name| -> Result<String, ()> {
                Ok(format!("<{name}>"))
            })
            .unwrap();
        assert_eq!(out, "token <GITHUB_TOKEN> and <OTHER>!");
        assert!(!Sentinel::Secret.present_in("plain"));
        assert_eq!(
            Sentinel::Secret
                .resolve_in("plain", |_| -> Result<String, ()> { unreachable!() })
                .unwrap(),
            "plain"
        );
        let failed: Result<String, &str> =
            Sentinel::Secret.resolve_in(&Sentinel::secret("X"), |_| Err("missing"));
        assert_eq!(failed, Err("missing"));

        let literal = format!(
            "{}X{SENTINEL_CLOSE}{SENTINEL_ESCAPE}",
            Sentinel::Secret.open()
        );
        let escaped = escape_sentinel_text(&literal);
        assert!(!Sentinel::Secret.present_in(&escaped));
        assert_eq!(unescape_sentinel_text(&escaped), literal);
    }

    #[test]
    fn constant_markers_replace_only_when_present() {
        let text = format!("cd {}/sub", Sentinel::WORKSPACE_MARKER);
        assert!(Sentinel::Workspace.present_in(&text));
        assert_eq!(
            Sentinel::Workspace.replace_in(&text, "/w/repo"),
            Some("cd /w/repo/sub".to_string())
        );
        assert_eq!(Sentinel::Workspace.replace_in("plain", "/w/repo"), None);
        assert!(!Sentinel::RunnerTemp.present_in(&text));
    }

    #[test]
    fn hashfiles_sentinels_carry_their_patterns() {
        let patterns = vec!["**/Cargo.lock".to_string(), "rust-toolchain*".to_string()];
        let text = format!("key-{}-v1", Sentinel::hashfiles(&patterns));
        assert!(Sentinel::HashFiles.present_in(&text));
        assert!(!Sentinel::Secret.present_in(&text));
        assert_eq!(Sentinel::hashfiles_calls(&text), vec![patterns.clone()]);
        let out = Sentinel::resolve_hashfiles(&text, |p| -> Result<String, ()> {
            assert_eq!(p, patterns.as_slice());
            Ok("abc123".into())
        })
        .unwrap();
        assert_eq!(out, "key-abc123-v1");
        // The two sentinel kinds pass each other by.
        let mixed = format!(
            "{} {}",
            Sentinel::secret("T"),
            Sentinel::hashfiles(&patterns)
        );
        let out = Sentinel::Secret
            .resolve_in(&mixed, |_| -> Result<String, ()> { Ok("s".into()) })
            .unwrap();
        assert!(Sentinel::HashFiles.present_in(&out));

        let patterns = vec![format!("a{SENTINEL_CLOSE}b")];
        let marker = Sentinel::hashfiles(&patterns);
        assert_eq!(Sentinel::hashfiles_calls(&marker), vec![patterns]);
    }
}
