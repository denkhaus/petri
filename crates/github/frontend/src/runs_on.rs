//! `runs-on` with expressions, resolved at lowering — per matrix leg.
//!
//! GitHub evaluates a job's `runs-on` when the job is queued, with that leg's
//! `matrix` in scope. A lowering has the same information whenever the matrix is
//! static, so the labels resolve here, once per leg, with the engine's own
//! machinery: the matrix expands through the same combinators
//! ([`crate::expr_lower::matrix_legs`]) and the expression evaluates through the
//! engine's evaluator, in a private table the graph never sees. The `inputs`
//! context resolves the same way from the values known at lowering — a literal
//! call site's `with:`, a declared default — so a called workflow's
//! `runs-on: ${{ inputs.runner }}` places per call site. What cannot be
//! resolved — a run-time context, an input the call site computes, a matrix
//! whose legs are themselves expressions — is a specific diagnostic, not a
//! guess.

use frontend::diag::Span;
use frontend::expr::lower::{LowerError, Roots};
use frontend::expr::{parse, split_template};
use frontend::yaml::Node;
use ir::expr::{EvalEnv, StaticCtx, eval};
use ir::flow::RunContext;
use ir::{ExprId, ExprTable, Value};

/// The matrix's static legs: the literal matrix through the same expansion the
/// engine runs at firing time, evaluated now. `None` when any value carries an
/// expression — the legs are then unknown before the run.
pub fn static_legs(matrix: Node<'_>) -> Option<Vec<Value>> {
    if has_expression(matrix) {
        return None;
    }
    let mut table = ExprTable::new();
    let m = table.lit(matrix.to_json());
    let legs = crate::expr_lower::matrix_legs(&mut table, m).ok()?;
    let run = RunContext::new();
    let statics = StaticCtx::new();
    let env = EvalEnv::new(&Value::Null, &run, &statics);
    match eval(&table, legs, &env) {
        Ok(Value::Array(legs)) => Some(legs),
        _ => None,
    }
}

fn has_expression(node: Node<'_>) -> bool {
    if let Some(m) = node.as_mapping() {
        return m.iter().any(|(_, v)| has_expression(v));
    }
    if let Some(s) = node.as_sequence() {
        return s.iter().any(has_expression);
    }
    node.as_str().is_some_and(|t| t.contains("${{"))
}

/// One label position of a `runs-on`, as the reader collected it: the text as
/// written, where it sits, and whether it was the whole scalar value — a whole
/// value may evaluate to a list of labels, an element of a written list may not.
pub struct RawLabel {
    pub text: String,
    pub span: Span,
    pub whole: bool,
}

impl RawLabel {
    /// The label at one node, when the node is a scalar.
    pub fn from_node(node: Node<'_>, whole: bool) -> Option<Self> {
        node.as_str().map(|s| Self {
            text: s.to_string(),
            span: node.span(),
            whole,
        })
    }
}

/// Why a `runs-on` cannot resolve at lowering. Every case lands in one
/// `runs_on.expression` rejection whose message carries the specifics.
pub enum Failure {
    /// The expression reads a context that has no value before the run.
    RunTimeContext { name: String, span: Span },
    /// The expression reads an input whose value the call site computes at run
    /// time, so no lowering can place it.
    DynamicInput { name: String, span: Span },
    /// The expression does not parse, lower, or evaluate.
    Bad { message: String, span: Span },
    /// It evaluated, but not to a label or list of labels.
    NotLabels { got: String, span: Span },
}

/// The marker a dynamic input's placeholder value carries, so a resolved label
/// that absorbed one is caught — and named — rather than placed. Never a
/// character a real label contains.
pub const DYNAMIC_MARK: char = '\u{1}';

/// A compiled `runs-on`: each label position lowered once, evaluated once per
/// leg. The table is private — nothing of this resolution enters the graph.
pub struct Compiled {
    table: ExprTable,
    entries: Vec<Entry>,
}

struct Entry {
    id: ExprId,
    span: Span,
    whole: bool,
}

/// The contexts a `runs-on` expression may read before the run: the leg's
/// `matrix`, and the frame's `inputs`.
struct StaticContexts;

impl Roots for StaticContexts {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        let lowered = name.to_lowercase();
        matches!(lowered.as_str(), "matrix" | "inputs").then(|| table.var(&lowered))
    }
}

/// Lower every label position. Fails on the first expression that reads past
/// `matrix` — that verdict is per job, not per leg.
pub fn compile(raw: &[RawLabel]) -> Result<Compiled, Failure> {
    let mut table = ExprTable::new();
    let mut entries = Vec::with_capacity(raw.len());
    for label in raw {
        let id = compile_scalar(&label.text, &label.span, &mut table)?;
        entries.push(Entry {
            id,
            span: label.span.clone(),
            whole: label.whole,
        });
    }
    Ok(Compiled { table, entries })
}

/// One scalar as an expression: a literal stays itself, a whole `${{ … }}`
/// keeps its value, and a mixed template concatenates stringified pieces —
/// through the same fold ([`crate::exprs::fold_template`]) that shapes the
/// graph's templates, here in the private table.
fn compile_scalar(text: &str, span: &Span, table: &mut ExprTable) -> Result<ExprId, Failure> {
    let bad = |message: String| Failure::Bad {
        message,
        span: span.clone(),
    };
    if !text.contains("${{") {
        return Ok(table.lit(text));
    }
    let segments = split_template(text).map_err(|_| bad("unterminated `${{`".into()))?;
    crate::exprs::fold_template(
        &segments,
        table,
        |t| t.to_string(),
        |source, table| {
            let ast = parse(source)
                .map_err(|e| bad(format!("could not parse `{}`: {e}", source.trim())))?;
            crate::expr_lower::gha(&ast, table, &mut StaticContexts).map_err(|e| match e {
                LowerError::UnknownIdent(name) => Failure::RunTimeContext {
                    name,
                    span: span.clone(),
                },
                other => bad(other.to_string()),
            })
        },
        &bad,
    )
}

impl Compiled {
    /// The labels one leg resolves to, each with the span of the position that
    /// produced it. `inputs` is the frame's statically-known values, with
    /// [`DYNAMIC_MARK`] placeholders standing in for run-time ones — a label
    /// that absorbed a placeholder names its input instead of placing.
    pub fn labels_for(&self, leg: &Value, inputs: &Value) -> Result<Vec<(String, Span)>, Failure> {
        let statics = StaticCtx::new()
            .bind("matrix", leg.clone())
            .bind("inputs", inputs.clone());
        let run = RunContext::new();
        let env = EvalEnv::new(&Value::Null, &run, &statics);
        let mut labels = Vec::new();
        for entry in &self.entries {
            let value = eval(&self.table, entry.id, &env).map_err(|e| Failure::Bad {
                message: e.to_string(),
                span: entry.span.clone(),
            })?;
            let not_labels = |got: &Value| Failure::NotLabels {
                got: got.to_string(),
                span: entry.span.clone(),
            };
            let mut push = |s: String| -> Result<(), Failure> {
                if let Some(name) = s.split(DYNAMIC_MARK).nth(1) {
                    return Err(Failure::DynamicInput {
                        name: name
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                            .collect(),
                        span: entry.span.clone(),
                    });
                }
                labels.push((s, entry.span.clone()));
                Ok(())
            };
            match value {
                Value::String(s) if !s.trim().is_empty() => push(s)?,
                Value::Array(items) if entry.whole => {
                    for item in items {
                        match item {
                            Value::String(s) if !s.trim().is_empty() => push(s)?,
                            other => return Err(not_labels(&other)),
                        }
                    }
                }
                other => return Err(not_labels(&other)),
            }
        }
        Ok(labels)
    }
}
