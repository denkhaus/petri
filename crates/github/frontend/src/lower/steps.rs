//! `run:` steps: one node per step with its env, shell and budget, and the
//! gate lowered into the node's config for the step kind to evaluate.

use std::time::Duration;

use frontend::diag::{Diagnostics, Span};
use frontend::expr::{Expr, parse};
use frontend::yaml::Node;
use ir::placeholder::{EXPR_PLACEHOLDER_KEY, SECRET_REF_KEY};
use ir::{BinOp, Budget, ExprId, ExprOrValue, NodeId, ScopeId, StepRef, Value};
use serde_json::{Map, json};

use super::{ActionPlan, EnvValue, Lowering, if_expr_source, names_status_function};
use crate::action::RUN_KIND;
use crate::exprs::{LoweredScalar, SEP, Site, config_value, lower_scalar};
use crate::gate::{self, Gate, GateOp};
use crate::model::{Defaults, Job, Step};

/// GitHub's default job timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_hours(6);

impl<'a> Lowering<'_, 'a> {
    /// The node(s) for one step: one `github/run` node, one `github/action`
    /// node, or a composite's inlined chain.
    pub(super) fn step_nodes(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        scope: ScopeId,
        site: &mut Site,
        earlier: &[String],
        job_secret_env: &[(String, String)],
        inherited: Defaults<'_>,
        depth: usize,
        action_plan: Option<&ActionPlan>,
    ) -> Vec<NodeId> {
        let node_name = if site.action_inputs.is_some() {
            format!("{}{SEP}{}", site.job_id, step.node_name())
        } else {
            format!("{}{SEP}{}", job.id, step.node_name())
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
                action_plan,
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
        if let Some(v) = self.soft_fail_value(step.continue_on_error, &step_site, false) {
            config.insert("soft_fail".into(), v);
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

    /// `continue-on-error` → the step's `soft_fail` config value. A literal
    /// bool stays literal. An expression lowers and resolves when the step
    /// fires — GitHub evaluates it at step time, so `matrix.*` and `steps.*`
    /// are visible per leg — wrapped in `loose_truthy` so the resolved value
    /// is always a bool for the step's `SoftFail`. `quiet` suppresses
    /// diagnostics where a lifecycle phase repeats the main phase's value.
    pub(super) fn soft_fail_value(
        &mut self,
        coe: Option<Node<'_>>,
        site: &Site,
        quiet: bool,
    ) -> Option<Value> {
        let coe = coe?;
        if let Some(b) = coe.as_scalar().and_then(|s| s.as_bool()) {
            return b.then(|| json!(true));
        }
        let Some(text) = coe.as_str() else {
            if !quiet {
                self.diags.unsupported(
                    "continue_on_error.expression",
                    coe.span(),
                    "a non-boolean `continue-on-error`",
                    "use a boolean, or an expression evaluating to one",
                );
            }
            return None;
        };
        let mut scratch = Diagnostics::new();
        let lowered = {
            let diags = if quiet { &mut scratch } else { &mut self.diags };
            lower_scalar(text, coe.span(), site, true, false, self.b.exprs(), diags)?
        };
        match lowered {
            LoweredScalar::Expr(id) => {
                let t = self.b.exprs();
                let truthy = t.call("loose_truthy", vec![id]);
                Some(json!({ EXPR_PLACEHOLDER_KEY: truthy.raw() }))
            }
            LoweredScalar::Literal(v) => match v.as_str() {
                Some("true") => Some(json!(true)),
                Some("false" | "") => None,
                _ => {
                    if !quiet {
                        self.diags.unsupported(
                            "continue_on_error.expression",
                            coe.span(),
                            "a non-boolean `continue-on-error` value",
                            "use true, false, or an expression evaluating to one",
                        );
                    }
                    None
                }
            },
            LoweredScalar::Secret(_) => {
                if !quiet {
                    self.diags.unsupported(
                        "continue_on_error.expression",
                        coe.span(),
                        "a secret-valued `continue-on-error`",
                        "use a boolean expression",
                    );
                }
                None
            }
        }
    }

    /// The step's `env:` as config, secrets from the job pushed down first.
    /// Step env is also made visible to the step's own expressions through
    /// `site`.
    pub(super) fn step_env_config(
        &mut self,
        step: &Step<'_>,
        step_site: &mut Site,
        job_secret_env: &[(String, String)],
    ) -> Map<String, Value> {
        let mut env_config = Map::new();
        for (key, name) in job_secret_env {
            env_config.insert(key.clone(), json!({ SECRET_REF_KEY: name }));
        }
        for (key, node) in &step.env {
            match self.env_value(node, step_site, true) {
                Some(EnvValue::Plain(v)) => {
                    step_site.step_env.insert(key.clone(), v.clone());
                    env_config.insert(key.clone(), match v {
                        ExprOrValue::Value(v) => v,
                        ExprOrValue::Expr(id) => json!({ EXPR_PLACEHOLDER_KEY: id.raw() }),
                    });
                }
                Some(EnvValue::Secret(name)) => {
                    env_config.insert(key.clone(), json!({ SECRET_REF_KEY: name }));
                }
                None => {}
            }
        }
        env_config
    }

    /// Timeout: step, else job, else GitHub's default.
    pub(super) fn set_step_budget(&mut self, id: NodeId, job: &Job<'a>, step: &Step<'_>) {
        let timeout = self
            .minutes(step.timeout_minutes)
            .or_else(|| self.minutes(job.timeout_minutes))
            .unwrap_or(DEFAULT_TIMEOUT);
        self.b.set_budget(id, Budget::new(1, timeout));
    }

    /// A main step's gate: the job started, and the step's own condition
    /// (default `success()` over earlier steps), attached to the node's
    /// config for the step kind to evaluate at spawn. The node carries no
    /// engine precondition.
    pub(super) fn gate_main_node(&mut self, id: NodeId, step: &Step<'_>, step_site: &Site) {
        let started = step_site.job_started(self.b.exprs());
        let gate = self.step_gate(step.condition, step_site, step.span.clone(), &[started]);
        self.attach_gate(id, gate);
    }

    /// The step-level condition as a gate. `prereqs` are engine-side terms
    /// ANDed in front — the job-started term, a post node's main-ran term.
    ///
    /// A condition with nothing only the step can resolve — no `env.*`, no
    /// `hashFiles` — collapses with the prerequisites into a single engine
    /// expression: one `$expr` leaf, the common case. Otherwise the condition
    /// splits on its operators into a gate tree ([`gate::condition_tree`]).
    pub(super) fn step_gate(
        &mut self,
        node: Option<Node<'_>>,
        site: &Site,
        span: Span,
        prereqs: &[ExprId],
    ) -> Value {
        if let Some(scalar) = node.and_then(|n| n.as_scalar())
            && scalar.as_bool().is_none()
            && let Ok(source) = if_expr_source(scalar.as_str())
        {
            return self.step_gate_text(&source, site, span, prereqs);
        }
        let cond = self.condition(node, site, true, span);
        self.collapse_gate(prereqs, cond)
    }

    /// [`Self::step_gate`] over bare condition text (a `pre-if`, a `post-if`).
    pub(super) fn step_gate_text(
        &mut self,
        source: &str,
        site: &Site,
        span: Span,
        prereqs: &[ExprId],
    ) -> Value {
        if let Ok(ast) = parse(source)
            && gate::needs_lazy(&ast)
        {
            return self.lazy_gate(&ast, site, &span, prereqs);
        }
        let cond = self.condition_text(source, site, true, span);
        self.collapse_gate(prereqs, cond)
    }

    /// The all-engine case: one `$expr` leaf holding `prereq && … &&
    /// condition`.
    fn collapse_gate(&mut self, prereqs: &[ExprId], cond: Option<ExprId>) -> Value {
        let mut acc: Option<ExprId> = None;
        for term in prereqs.iter().copied().chain(cond) {
            acc = Some(match acc {
                None => term,
                Some(a) => self.b.exprs().binary(BinOp::And, a, term),
            });
        }
        let id = acc.unwrap_or_else(|| self.b.exprs().lit(true));
        Gate::expr(id).to_value()
    }

    /// The gate tree for a condition with step-resolved leaves: prerequisites
    /// and the implicit `success()` (unless the condition names a status
    /// function) as engine leaves, then the condition split on its
    /// operators. GitHub truthiness lands at the root, in the step's
    /// evaluator.
    fn lazy_gate(&mut self, ast: &Expr, site: &Site, span: &Span, prereqs: &[ExprId]) -> Value {
        let mut terms: Vec<Gate> = prereqs.iter().map(|id| Gate::expr(*id)).collect();
        if !names_status_function(ast) {
            let success = site.status_function(self.b.exprs(), "success", true);
            terms.push(Gate::expr(success));
        }
        if let Some(tree) = gate::condition_tree(ast, site, span, self.b.exprs(), &mut self.diags) {
            terms.push(tree);
        }
        let gate = match terms.len() {
            1 => terms.pop().expect("one term"),
            _ => Gate::Op {
                op:   GateOp::And,
                args: terms,
            },
        };
        gate.to_value()
    }

    /// The engine's `scope_cancelled` static as a config placeholder: the bit
    /// that turns a false gate into a `Cancelled` record rather than a
    /// `Skipped` one, mirroring how the engine records a false precondition
    /// in a cancelled scope.
    pub(super) fn scope_cancelled_config(&mut self) -> Value {
        let id = self.b.exprs().var("scope_cancelled");
        json!({ EXPR_PLACEHOLDER_KEY: id.raw() })
    }

    /// Put the gate (and the cancelled bit) into a step node's config.
    pub(super) fn attach_gate(&mut self, id: NodeId, gate: Value) {
        let cancelled = self.scope_cancelled_config();
        let node = self.b.node_mut(id);
        if let Value::Object(map) = &mut node.step.config {
            map.insert("gate".into(), gate);
            map.insert("cancelled".into(), cancelled);
        }
    }

    /// `github.event`, for `GITHUB_EVENT_PATH`.
    pub(super) fn event_config(&mut self) -> Value {
        let t = self.b.exprs();
        let github = t.var("github");
        let key = t.lit("event");
        let event = t.call("get_ci", vec![github, key]);
        json!({ EXPR_PLACEHOLDER_KEY: event.raw() })
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
}
