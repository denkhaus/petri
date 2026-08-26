//! What GitHub's contexts mean in engine terms.
//!
//! An expression in a workflow file sits somewhere — a job's `if:`, a step's `run:`,
//! a composite action's output — and what `steps.build.outputs.x` or `success()` means
//! depends on where. [`Site`] is that "where". [`GhaRoots`] turns a context reference
//! into an engine expression for it, and the whole thing rides on the ordinary GHA
//! lowering: there is no evaluator here, only construction.

use std::collections::BTreeMap;

use frontend::diag::{Diagnostics, Span};
use frontend::expr::lower::{LowerError, Roots, builtin};

use crate::expr_lower::gha;
use frontend::expr::{Expr, Segment, parse, split_template};
use ir::placeholder::{EXPR_PLACEHOLDER_KEY, SECRET_REF_KEY};
use ir::{BinOp, ExprId, ExprOrValue, ExprTable, UnOp, Value};
use serde_json::json;

/// The node-name separator between a job and its steps, and between a composite
/// caller and its inner steps. Node names are `job/step`, `job/caller/inner`.
pub const SEP: char = '/';

/// A GitHub status tag as the engine records it, mapped to how GitHub reports it.
/// The engine has `partial_success` and `timed_out`; GitHub has neither.
pub fn outcome_tag(table: &mut ExprTable, status: ExprId) -> ExprId {
    // outcome: the result before continue-on-error — partial_success reads as failure.
    remap_status(
        table,
        status,
        &[("partial_success", "failure"), ("timed_out", "failure")],
    )
}

pub fn conclusion_tag(table: &mut ExprTable, status: ExprId) -> ExprId {
    // conclusion: after continue-on-error — partial_success reads as success.
    remap_status(
        table,
        status,
        &[("partial_success", "success"), ("timed_out", "failure")],
    )
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

/// Where an expression sits.
#[derive(Clone)]
pub struct Site {
    pub job_id: String,
    /// `job/step` names of the steps before this one in the job, in order.
    pub earlier_steps: Vec<String>,
    /// Every step of this job, for `steps.<id>` lookups: id → node name.
    pub step_names: BTreeMap<String, String>,
    /// The job's `needs`, with each needed job's `done` node name.
    pub needs: BTreeMap<String, String>,
    /// The job is a matrix job, so node names take a `#index` suffix at run time.
    pub matrix: bool,
    /// Static matrix leg count, when the matrix is a literal.
    pub matrix_total: Option<usize>,
    pub fail_fast: bool,
    pub max_parallel: Option<u32>,
    /// Step-level `env`, for `env.X` inside the step.
    pub step_env: BTreeMap<String, ExprOrValue>,
    /// Composite action inputs in scope: input name → already-lowered value.
    pub action_inputs: Option<BTreeMap<String, ExprId>>,
    /// Composite action outputs the caller can see: step id → (output name → expr).
    pub composite_outputs: BTreeMap<String, BTreeMap<String, ExprId>>,
    /// The job's own `start` node name (its gate).
    pub start_node: String,
}

impl Site {
    pub fn new(job_id: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            earlier_steps: Vec::new(),
            step_names: BTreeMap::new(),
            needs: BTreeMap::new(),
            matrix: false,
            matrix_total: None,
            fail_fast: true,
            max_parallel: None,
            step_env: BTreeMap::new(),
            action_inputs: None,
            composite_outputs: BTreeMap::new(),
            start_node: format!("{job_id}{SEP}start"),
        }
    }

    /// The run-context record for a node of this job, as an expression: static for a
    /// plain job, `nodes[name + '#' + index]` inside a matrix clone.
    pub fn node_record(&self, table: &mut ExprTable, name: &str) -> ExprId {
        let nodes = table.var("nodes");
        if self.matrix {
            let prefix = table.lit(format!("{name}#"));
            let index = table.var("index");
            let key = table.binary(BinOp::Add, prefix, index);
            table.index(nodes, key)
        } else {
            table.field(nodes, name)
        }
    }

    pub fn node_status(&self, table: &mut ExprTable, name: &str) -> ExprId {
        let record = self.node_record(table, name);
        table.field(record, "status")
    }

    /// `nodes[...].status == 'tag'`.
    pub fn node_has_status(&self, table: &mut ExprTable, name: &str, tag: &str) -> ExprId {
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
    pub fn earlier_step_failed(&self, table: &mut ExprTable) -> ExprId {
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

    pub fn earlier_step_cancelled(&self, table: &mut ExprTable) -> ExprId {
        let terms: Vec<ExprId> = self
            .earlier_steps
            .iter()
            .map(|name| self.node_has_status(table, name, "cancelled"))
            .collect();
        Self::any_of(table, terms)
    }

    /// The job was not skipped by its own gate.
    pub fn job_started(&self, table: &mut ExprTable) -> ExprId {
        let skipped = self.node_has_status(table, &self.start_node, "skipped");
        table.unary(UnOp::Not, skipped)
    }

    /// A needed job's result: `nodes["N/done"].output.result`.
    pub fn need_result(&self, table: &mut ExprTable, done_node: &str) -> ExprId {
        let nodes = table.var("nodes");
        let record = table.field(nodes, done_node);
        let output = table.field(record, "output");
        table.field(output, "result")
    }

    fn need_has_result(&self, table: &mut ExprTable, done_node: &str, tag: &str) -> ExprId {
        let result = self.need_result(table, done_node);
        let lit = table.lit(tag);
        table.binary(BinOp::Eq, result, lit)
    }

    /// Job-level `success()`: every needed job succeeded.
    pub fn needs_succeeded(&self, table: &mut ExprTable) -> ExprId {
        let mut acc = table.lit(true);
        for done in self.needs.values() {
            let ok = self.need_has_result(table, done, "success");
            acc = table.binary(BinOp::And, acc, ok);
        }
        acc
    }

    pub fn needs_failed(&self, table: &mut ExprTable) -> ExprId {
        let terms: Vec<ExprId> = self
            .needs
            .values()
            .map(|done| self.need_has_result(table, done, "failure"))
            .collect();
        Self::any_of(table, terms)
    }

    pub fn needs_cancelled(&self, table: &mut ExprTable) -> ExprId {
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
    /// `cancelled()` also ORs in the engine's `scope_cancelled` static: a cancel
    /// that lands between steps cancels no step record, a not-yet-started job has
    /// no cancelled needs, and a `fail_fast` splice cancel is not a root cancel —
    /// `scope_cancelled` covers all three.
    pub fn status_function(&self, table: &mut ExprTable, name: &str, at_step: bool) -> ExprId {
        match (name, at_step) {
            ("always", _) => table.lit(true),
            ("success", true) => {
                let failed = self.earlier_step_failed(table);
                let cancelled = self.earlier_step_cancelled(table);
                let bad = table.binary(BinOp::Or, failed, cancelled);
                table.unary(UnOp::Not, bad)
            }
            ("failure", true) => self.earlier_step_failed(table),
            ("cancelled", true) => {
                let earlier = self.earlier_step_cancelled(table);
                let scoped = table.var("scope_cancelled");
                table.binary(BinOp::Or, earlier, scoped)
            }
            ("success", false) => self.needs_succeeded(table),
            ("failure", false) => self.needs_failed(table),
            ("cancelled", false) => {
                let needs = self.needs_cancelled(table);
                let scoped = table.var("scope_cancelled");
                table.binary(BinOp::Or, needs, scoped)
            }
            _ => table.lit(false),
        }
    }
}

/// How the lowering resolves GitHub's contexts.
pub struct GhaRoots<'s> {
    pub site: &'s Site,
    pub at_step: bool,
    pub diags: &'s mut Diagnostics,
    pub span: Span,
    /// Set when a `secrets.*` reference was seen; the caller decides whether the
    /// position allowed it.
    pub saw_secret: bool,
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
                self.saw_secret = true;
                // Lowered to a marker the caller checks for; it never evaluates.
                Some(table.lit(Value::Null))
            }
            "inputs" => match &self.site.action_inputs {
                Some(_) => Some(table.lit(Value::Null)),
                None => {
                    self.diags.unsupported(
                        "inputs",
                        self.span.clone(),
                        "the `inputs` context is only populated for `workflow_dispatch` and `workflow_call`",
                        "both are v2",
                    );
                    // Lower to null so the rejection above is the one diagnostic.
                    Some(table.lit(Value::Null))
                }
            },
            "steps" | "needs" | "job" | "strategy" => Some(table.lit(Value::Null)),
            "jobs" => {
                self.diags.unsupported(
                    "jobs_context",
                    self.span.clone(),
                    "the `jobs` context exists only in reusable workflows",
                    "reusable workflows are v2",
                );
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
                match what.to_lowercase().as_str() {
                    "result" => Some(self.site.need_result(table, &done)),
                    "outputs" => {
                        let nodes = table.var("nodes");
                        let record = table.field(nodes, &done);
                        let output = table.field(record, "output");
                        let mut id = table.field(output, "outputs");
                        for key in rest {
                            id = table.field(id, key);
                        }
                        Some(id)
                    }
                    _ => Some(table.lit(Value::Null)),
                }
            }
            "job" => match path.first().map(|s| s.to_lowercase()).as_deref() {
                Some("status") => {
                    let failed = self.site.earlier_step_failed(table);
                    let cancelled = self.site.earlier_step_cancelled(table);
                    let f = table.lit("failure");
                    let c = table.lit("cancelled");
                    let s = table.lit("success");
                    let inner = table.cond(cancelled, c, s);
                    Some(table.cond(failed, f, inner))
                }
                Some("container") | Some("services") => {
                    self.diags.unsupported(
                        "job_context",
                        self.span.clone(),
                        format!("`job.{}` describes service containers", path[0]),
                        "service containers are v2",
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
                let inputs = self.site.action_inputs.as_ref()?;
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
            "secrets" => {
                self.saw_secret = true;
                Some(table.lit(Value::Null))
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
                        name: name.to_string(),
                        expected: 0,
                        got: args.len(),
                    }));
                }
                Some(Ok(self.site.status_function(table, n, self.at_step)))
            }
            "hashfiles" => {
                self.diags.unsupported(
                    "expression.hashFiles",
                    self.span.clone(),
                    "`hashFiles()` reads the workspace at run time",
                    "no pure expression can do that; it needs a resolution-time placeholder like `$secret`, \
                     which is a spec finding rather than a frontend feature",
                );
                Some(Err(LowerError::Custom("hashFiles is not supported".into())))
            }
            _ => None,
        }
    }
}

/// How a scalar lowered.
pub enum LoweredScalar {
    /// A plain value, no expression in it.
    Literal(Value),
    /// An expression.
    Expr(ExprId),
    /// The whole value was `${{ secrets.NAME }}`.
    Secret(String),
}

/// Lower one scalar from the workflow: literal, templated string, or a whole
/// expression. `env_shaped` says whether a bare `${{ secrets.X }}` is allowed here.
pub fn lower_scalar(
    text: &str,
    span: Span,
    site: &Site,
    at_step: bool,
    env_shaped: bool,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<LoweredScalar> {
    let segments = match split_template(text) {
        Ok(s) => s,
        Err(_) => {
            diags.error("expr.unterminated", span, "unterminated `${{`");
            return None;
        }
    };
    if !text.contains("${{") {
        return Some(LoweredScalar::Literal(Value::String(text.to_string())));
    }

    // A whole-value secret reference is the one permitted form.
    if let [Segment::Expr { source, .. }] = segments.as_slice()
        && let Ok(ast) = parse(source)
        && let Some((root, path)) = ast.dotted_path()
        && root.eq_ignore_ascii_case("secrets")
        && path.len() == 1
    {
        if env_shaped {
            return Some(LoweredScalar::Secret(path[0].to_string()));
        }
        diags.unsupported(
                        "secrets.expression",
                        span,
                        format!("`secrets.{}` used as an expression rather than as the whole value of an `env:` or `with:` entry", path[0]),
                        "secrets are absent from the expression environment by construction, so they never reach the \
                         event log; pass the secret through an environment variable and test it in the step",
                    );
        return None;
    }

    let whole = matches!(segments.as_slice(), [Segment::Expr { .. }]);
    let mut pieces: Vec<ExprId> = Vec::new();
    for segment in &segments {
        match segment {
            Segment::Text(t) => {
                let id = table.lit(t.as_str());
                pieces.push(id);
            }
            Segment::Expr { source, .. } => {
                let ast = match parse(source) {
                    Ok(ast) => ast,
                    Err(e) => {
                        diags.error(
                            "expr.parse",
                            span,
                            format!("could not parse `${{{{ {} }}}}`: {e}", source.trim()),
                        );
                        return None;
                    }
                };
                let mut roots = GhaRoots {
                    site,
                    at_step,
                    diags,
                    span: span.clone(),
                    saw_secret: false,
                };
                let id = match gha(&ast, table, &mut roots) {
                    Ok(id) => id,
                    Err(LowerError::UnknownIdent(name)) => {
                        roots.diags.error(
                            "expr.unknown_context",
                            span,
                            format!("`{name}` is not a GitHub Actions context"),
                        );
                        return None;
                    }
                    Err(LowerError::Custom(_)) => return None,
                    Err(e) => {
                        roots.diags.error("expr.lower", span, e.to_string());
                        return None;
                    }
                };
                if roots.saw_secret {
                    roots.diags.unsupported(
                        "secrets.expression",
                        span,
                        "a `secrets.*` reference inside a larger expression or string",
                        "secrets are absent from the expression environment by construction, so they never reach the \
                         event log; a secret may only be the entire value of an `env:` or `with:` entry",
                    );
                    return None;
                }
                if whole {
                    return Some(LoweredScalar::Expr(id));
                }
                let as_string = builtin(table, "loose_string", vec![id]).ok()?;
                pieces.push(as_string);
            }
        }
    }
    let mut iter = pieces.into_iter();
    let mut acc = iter.next()?;
    for next in iter {
        acc = table.binary(BinOp::Concat, acc, next);
    }
    Some(LoweredScalar::Expr(acc))
}

/// A config value from a lowered scalar: literal, `{"$expr": id}`, or `{"$secret": name}`.
pub fn config_value(lowered: LoweredScalar) -> Value {
    match lowered {
        LoweredScalar::Literal(v) => v,
        LoweredScalar::Expr(id) => json!({ EXPR_PLACEHOLDER_KEY: id.raw() }),
        LoweredScalar::Secret(name) => json!({ SECRET_REF_KEY: name }),
    }
}
