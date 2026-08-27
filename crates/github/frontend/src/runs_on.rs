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

/// The matrix's static legs: the matrix through the same expansion the engine
/// runs at firing time, evaluated now. A value carrying an expression resolves
/// against the same static contexts `runs-on` reads — the frame's known
/// `inputs` and the checkout's `github` identity — so a matrix axis guarded on
/// the repository is as static as a literal one. `None` when a value stays
/// unknown before the run: the legs are then dynamic.
pub fn static_legs(matrix: Node<'_>, inputs: &Value, github: &Value) -> Option<Vec<Value>> {
    let mut table = ExprTable::new();
    let value = static_value(matrix, &mut table, inputs, github)?;
    let m = table.lit(value);
    let legs = crate::expr_lower::matrix_legs(&mut table, m).ok()?;
    let env_statics = statics(inputs, github, &Value::Null);
    let run = RunContext::new();
    let env = EvalEnv::new(&Value::Null, &run, &env_statics);
    match eval(&table, legs, &env) {
        Ok(Value::Array(legs)) => Some(legs),
        _ => None,
    }
}

/// One matrix node as a value: literals as themselves, expressions evaluated
/// over the static contexts. `None` — dynamic — when an expression fails to
/// resolve or resolves through a run-time input's placeholder.
fn static_value(
    node: Node<'_>,
    table: &mut ExprTable,
    inputs: &Value,
    github: &Value,
) -> Option<Value> {
    if let Some(m) = node.as_mapping() {
        let mut out = serde_json::Map::new();
        for (key, value) in m.iter() {
            out.insert(key.to_string(), static_value(value, table, inputs, github)?);
        }
        return Some(Value::Object(out));
    }
    if let Some(seq) = node.as_sequence() {
        return seq
            .iter()
            .map(|item| static_value(item, table, inputs, github))
            .collect::<Option<Vec<Value>>>()
            .map(Value::Array);
    }
    let Some(text) = node.as_str() else {
        return Some(node.to_json());
    };
    if !text.contains("${{") {
        return Some(node.to_json());
    }
    let id = compile_scalar(text, &node.span(), table).ok()?;
    let env_statics = statics(inputs, github, &Value::Null);
    let run = RunContext::new();
    let env = EvalEnv::new(&Value::Null, &run, &env_statics);
    let value = eval(table, id, &env).ok()?;
    (!carries_mark(&value)).then_some(value)
}

/// The evaluation bindings every static resolution shares.
fn statics(inputs: &Value, github: &Value, leg: &Value) -> StaticCtx {
    StaticCtx::new()
        .bind("matrix", leg.clone())
        .bind("inputs", inputs.clone())
        .bind("github", github.clone())
}

/// Whether a value absorbed a run-time input's placeholder anywhere.
fn carries_mark(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains(DYNAMIC_MARK),
        Value::Array(items) => items.iter().any(carries_mark),
        Value::Object(map) => map.values().any(carries_mark),
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
/// `matrix`, the frame's `inputs`, and the checkout's `github` identity.
struct StaticContexts;

impl Roots for StaticContexts {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        let lowered = name.to_lowercase();
        matches!(lowered.as_str(), "matrix" | "inputs" | "github").then(|| table.var(&lowered))
    }
}

/// The contexts a per-*scope* value may read: `inputs` and `github`, but never
/// `matrix` — a scope is shared by every leg, so a leg-varying value has no one
/// place to land.
struct ScopeContexts;

impl Roots for ScopeContexts {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        let lowered = name.to_lowercase();
        matches!(lowered.as_str(), "inputs" | "github").then(|| table.var(&lowered))
    }
}

/// One scalar resolved against the frame's static `inputs` and the checkout's
/// `github` identity — the resolution a per-scope value gets (a container
/// image, a registry username). `matrix` is deliberately out of reach, and a
/// run-time input's placeholder fails as [`Failure::DynamicInput`] rather than
/// leaking into the value.
pub fn static_scalar(
    text: &str,
    span: &Span,
    inputs: &Value,
    github: &Value,
) -> Result<String, Failure> {
    let mut table = ExprTable::new();
    let id = compile_with(text, span, &mut table, &mut ScopeContexts)?;
    let env_statics = statics(inputs, github, &Value::Null);
    let run = RunContext::new();
    let env = EvalEnv::new(&Value::Null, &run, &env_statics);
    let value = eval(&table, id, &env).map_err(|e| Failure::Bad {
        message: e.to_string(),
        span: span.clone(),
    })?;
    let text = match value {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if let Some(name) = text.split(DYNAMIC_MARK).nth(1) {
        return Err(Failure::DynamicInput {
            name: name
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                .collect(),
            span: span.clone(),
        });
    }
    Ok(text)
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
    compile_with(text, span, table, &mut StaticContexts)
}

fn compile_with(
    text: &str,
    span: &Span,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<ExprId, Failure> {
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
            crate::expr_lower::gha(&ast, table, roots).map_err(|e| match e {
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
    /// that absorbed a placeholder names its input instead of placing — and
    /// `github` is the checkout's declared identity.
    pub fn labels_for(
        &self,
        leg: &Value,
        inputs: &Value,
        github: &Value,
    ) -> Result<Vec<(String, Span)>, Failure> {
        let env_statics = statics(inputs, github, leg);
        let run = RunContext::new();
        let env = EvalEnv::new(&Value::Null, &run, &env_statics);
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
