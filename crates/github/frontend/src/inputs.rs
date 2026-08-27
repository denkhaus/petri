//! Typed workflow inputs: one binding path for `workflow_call` and
//! `workflow_dispatch`.
//!
//! A called workflow's inputs bind from the caller's `with:`, lowered in the
//! caller's own context; a directly run workflow's bind from the run's
//! parameters (`github.event.inputs`). Either way the declarations are the
//! same ([`InputDecl`]), a static value type-checks at lowering, a dynamic one
//! is coerced with the engine's own builtins, `required` is enforced, and the
//! declared default fills the gap — so the callee's `inputs` context carries
//! typed values wherever they came from.

use std::collections::BTreeMap;

use frontend::diag::{Diagnostics, Span};
use frontend::expr::lower::builtin;
use frontend::yaml::Node;
use ir::{ExprId, ExprTable, Value};

use crate::exprs::{LoweredScalar, Site, escape_sentinel_text, lower_scalar};
use crate::model::{InputDecl, InputType};

/// One frame's bound inputs: the expressions the `inputs` context resolves to,
/// and the subset whose values are already known at lowering — literal `with:`
/// values and declared defaults — which the per-leg `runs-on` resolver may
/// read.
#[derive(Default)]
pub struct BoundInputs {
    pub exprs: BTreeMap<String, ExprId>,
    pub statics: BTreeMap<String, Value>,
}

/// Bind a call's inputs: the caller's `with:` against the callee's
/// declarations. `what` names the callee for diagnostics; `span` is the call
/// site. Unknown `with:` keys are errors, as on GitHub.
pub fn bind_call_inputs(
    decls: &[InputDecl<'_>],
    with: &[(String, Node<'_>)],
    caller_site: &Site,
    what: &str,
    span: &Span,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> BoundInputs {
    let mut given: BTreeMap<String, Node<'_>> = BTreeMap::new();
    for (key, value) in with {
        let lowered = key.to_lowercase();
        if !decls.iter().any(|d| d.name.to_lowercase() == lowered) {
            diags.error(
                "gha.unknown_input",
                value.span(),
                format!("`{key}` is not an input `{what}` declares"),
            );
        }
        given.insert(lowered, *value);
    }

    let mut bound = BoundInputs::default();
    for decl in decls {
        let value = match given.get(&decl.name.to_lowercase()) {
            Some(node) => bind_value(decl, *node, caller_site, table, diags),
            None if decl.required => {
                diags.error(
                    "gha.missing_input",
                    span.clone(),
                    format!("`{what}` requires input `{}`", decl.name),
                );
                continue;
            }
            None => default_value(decl, table, diags).map(|(id, v)| (id, Some(v))),
        };
        if let Some((id, known)) = value {
            bound.exprs.insert(decl.name.clone(), id);
            if let Some(known) = known {
                bound.statics.insert(decl.name.clone(), known);
            }
        }
    }
    bound
}

/// Bind inputs for a directly run workflow — `workflow_dispatch`, or a
/// reusable file run on its own — from the run's parameters: each becomes
/// `github.event.inputs.<name>`, typed, with the declared default when the
/// parameter is absent.
pub fn bind_param_inputs(
    decls: &[&InputDecl<'_>],
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> BoundInputs {
    // `github.event.inputs`, once; each declaration reads one key off it.
    let params = ["event", "inputs"].iter().fold(table.var("github"), |acc, key| {
        let k = table.lit(*key);
        builtin(table, "get_ci", vec![acc, k]).expect("get_ci exists")
    });
    let mut bound = BoundInputs::default();
    for decl in decls {
        let name_key = table.lit(decl.name.as_str());
        let raw = builtin(table, "get_ci", vec![params, name_key]).expect("get_ci exists");
        // `default(raw, fallback)` answers the absent case; the coercion then
        // types whatever value won.
        let (fallback, known) = match default_value(decl, table, diags) {
            Some((id, v)) => (id, Some(v)),
            None => (table.lit(type_zero(&decl.ty)), None),
        };
        let value = table.call("default", vec![raw, fallback]);
        bound
            .exprs
            .insert(decl.name.clone(), coerce_dynamic(decl, value, table));
        // Placement is a lowering decision, so a directly run workflow places
        // by its declared defaults — the value a bare run gets — and only by
        // those: a defaultless input stays dynamic.
        if let (Some(known), true) = (known, decl.default.is_some()) {
            bound.statics.insert(decl.name.clone(), known);
        }
    }
    bound
}

/// One caller-provided value: a literal type-checks now; an expression lowers
/// in the caller's context and is coerced at evaluation time.
fn bind_value(
    decl: &InputDecl<'_>,
    node: Node<'_>,
    caller_site: &Site,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<(ExprId, Option<Value>)> {
    let text = node.as_str().unwrap_or("");
    if !text.contains("${{") {
        let value = coerce_static(decl, node, diags)?;
        return Some((table.lit(value.clone()), Some(value)));
    }
    // Secrets are rejected here (`env_shaped: false`): a call passes secrets
    // through `secrets:`, never `with:`, exactly as GitHub requires.
    match lower_scalar(text, node.span(), caller_site, false, false, table, diags)? {
        LoweredScalar::Literal(Value::String(s)) => {
            let value = Value::String(escape_sentinel_text(&s));
            Some((table.lit(value.clone()), Some(value)))
        }
        LoweredScalar::Literal(v) => Some((table.lit(v.clone()), Some(v))),
        LoweredScalar::Expr(id) => Some((coerce_dynamic(decl, id, table), None)),
        // Unreachable with `env_shaped: false`; be safe rather than quiet.
        LoweredScalar::Secret(_) => None,
    }
}

/// The declared default, typed, or the type's zero value when there is none —
/// GitHub's rule for an unset optional input (`false`, `0`, `""`).
fn default_value(
    decl: &InputDecl<'_>,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<(ExprId, Value)> {
    let value = match decl.default {
        Some(node) => coerce_static(decl, node, diags)?,
        None => type_zero(&decl.ty),
    };
    Some((table.lit(value.clone()), value))
}

fn type_zero(ty: &InputType) -> Value {
    match ty {
        InputType::Boolean => Value::Bool(false),
        InputType::Number => Value::from(0),
        _ => Value::String(String::new()),
    }
}

/// A literal value against its declared type. `None` means a diagnostic was
/// reported.
fn coerce_static(decl: &InputDecl<'_>, node: Node<'_>, diags: &mut Diagnostics) -> Option<Value> {
    let scalar = node.as_scalar();
    let text = node.as_str().unwrap_or("").to_string();
    let bad = |diags: &mut Diagnostics, wants: &str| {
        diags.error(
            "gha.input_type",
            node.span(),
            format!("input `{}` is `{wants}`, got `{text}`", decl.name),
        );
        None
    };
    match &decl.ty {
        InputType::Boolean => match scalar.and_then(|s| s.as_bool()) {
            Some(b) => Some(Value::Bool(b)),
            None => bad(diags, "boolean"),
        },
        InputType::Number => match scalar {
            Some(s) => match (s.as_i64(), s.as_f64()) {
                (Some(i), _) => Some(Value::from(i)),
                (_, Some(f)) => serde_json::Number::from_f64(f).map(Value::Number),
                _ => bad(diags, "number"),
            },
            None => bad(diags, "number"),
        },
        InputType::Choice(options) => {
            if !options.is_empty() && !options.iter().any(|o| o == &text) {
                diags.error(
                    "gha.input_type",
                    node.span(),
                    format!(
                        "input `{}` must be one of [{}], got `{text}`",
                        decl.name,
                        options.join(", ")
                    ),
                );
                return None;
            }
            Some(Value::String(escape_sentinel_text(&text)))
        }
        InputType::String | InputType::Environment => {
            Some(Value::String(escape_sentinel_text(&text)))
        }
    }
}

/// Coerce a dynamic value at evaluation time: booleans and numbers parse from
/// their string form (`from_json` over `loose_string` handles both a real
/// `true` and the `"true"` an event payload carries); everything else
/// stringifies.
fn coerce_dynamic(decl: &InputDecl<'_>, id: ExprId, table: &mut ExprTable) -> ExprId {
    let text = builtin(table, "loose_string", vec![id]).expect("loose_string exists");
    match decl.ty {
        InputType::Boolean | InputType::Number => {
            builtin(table, "from_json", vec![text]).expect("from_json exists")
        }
        _ => text,
    }
}
