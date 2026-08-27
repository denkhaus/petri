//! `runs-on` with expressions, resolved at lowering — per matrix leg.
//!
//! GitHub evaluates a job's `runs-on` when the job is queued, with that leg's
//! `matrix` in scope. A lowering has the same information whenever the matrix is
//! static, so the labels resolve here, once per leg, with the engine's own
//! machinery: the matrix expands through the same combinators
//! ([`crate::expr_lower::matrix_legs`]) and the expression evaluates through the
//! engine's evaluator, in a private table the graph never sees. What cannot be
//! resolved — a run-time context, a matrix whose legs are themselves
//! expressions — is a specific diagnostic, not a guess.

use frontend::diag::Span;
use frontend::expr::lower::{LowerError, Roots, builtin};
use frontend::expr::{Segment, parse, split_template};
use frontend::yaml::Node;
use ir::expr::{EvalEnv, StaticCtx, eval};
use ir::flow::RunContext;
use ir::{BinOp, ExprId, ExprTable, Value};

/// The matrix's static legs: the literal matrix through the same expansion the
/// engine runs at firing time, evaluated now. `None` when any value carries an
/// expression — the legs are then unknown before the run.
pub fn static_legs(matrix: Node<'_>) -> Option<Vec<Value>> {
    let value = matrix.to_json();
    if has_expression(&value) {
        return None;
    }
    let mut table = ExprTable::new();
    let m = table.lit(value);
    let legs = crate::expr_lower::matrix_legs(&mut table, m).ok()?;
    let run = RunContext::new();
    let statics = StaticCtx::new();
    let env = EvalEnv::new(&Value::Null, &run, &statics);
    match eval(&table, legs, &env) {
        Ok(Value::Array(legs)) => Some(legs),
        _ => None,
    }
}

fn has_expression(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("${{"),
        Value::Array(items) => items.iter().any(has_expression),
        Value::Object(map) => map.values().any(has_expression),
        _ => false,
    }
}

/// One label position of a `runs-on`, as the reader collected it: the text as
/// written, where it sits, and whether it was the whole scalar value — a whole
/// value may evaluate to a list of labels, an element of a written list may not.
pub struct RawLabel {
    pub text: String,
    pub span: Span,
    pub whole: bool,
}

/// Why a `runs-on` cannot resolve at lowering. Every case lands in one
/// `runs_on.expression` rejection whose message carries the specifics.
pub enum Failure {
    /// The expression reads a context that has no value before the run.
    RunTimeContext { name: String, span: Span },
    /// The expression does not parse, lower, or evaluate.
    Bad { message: String, span: Span },
    /// It evaluated, but not to a label or list of labels.
    NotLabels { got: String, span: Span },
}

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

/// The one context a `runs-on` expression may read before the run.
struct MatrixOnly;

impl Roots for MatrixOnly {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        name.eq_ignore_ascii_case("matrix")
            .then(|| table.var("matrix"))
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
/// keeps its value, and a mixed template concatenates stringified pieces — the
/// same shape `lower_scalar` gives the graph, here in the private table.
fn compile_scalar(text: &str, span: &Span, table: &mut ExprTable) -> Result<ExprId, Failure> {
    let bad = |message: String| Failure::Bad {
        message,
        span: span.clone(),
    };
    let segments = split_template(text).map_err(|_| bad("unterminated `${{`".into()))?;
    if !text.contains("${{") {
        return Ok(table.lit(text));
    }
    let lower = |source: &str, table: &mut ExprTable| -> Result<ExprId, Failure> {
        let ast = parse(source).map_err(|e| bad(format!("could not parse `{}`: {e}", source.trim())))?;
        crate::expr_lower::gha(&ast, table, &mut MatrixOnly).map_err(|e| match e {
            LowerError::UnknownIdent(name) => Failure::RunTimeContext {
                name,
                span: span.clone(),
            },
            other => bad(other.to_string()),
        })
    };
    if let [Segment::Expr { source, .. }] = segments.as_slice() {
        return lower(source, table);
    }
    let mut pieces = Vec::with_capacity(segments.len());
    for segment in &segments {
        match segment {
            Segment::Text(t) => pieces.push(table.lit(t.clone())),
            Segment::Expr { source, .. } => {
                let id = lower(source, table)?;
                let as_string = builtin(table, "loose_string", vec![id])
                    .map_err(|e| bad(e.to_string()))?;
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

impl Compiled {
    /// The labels one leg resolves to, each with the span of the position that
    /// produced it.
    pub fn labels_for(&self, leg: &Value) -> Result<Vec<(String, Span)>, Failure> {
        let statics = StaticCtx::new().bind("matrix", leg.clone());
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
            match value {
                Value::String(s) if !s.trim().is_empty() => labels.push((s, entry.span.clone())),
                Value::Array(items) if entry.whole => {
                    for item in items {
                        match item {
                            Value::String(s) if !s.trim().is_empty() => {
                                labels.push((s, entry.span.clone()));
                            }
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
