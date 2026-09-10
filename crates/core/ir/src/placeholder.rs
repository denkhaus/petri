//! The two markers a step config may carry across a boundary.
//!
//! `{"$expr": id}` is a HIR placeholder: lowering leaves one where a value is
//! still an expression, and the engine resolves it against the firing's
//! environment before the step runs. `{"$secret": "NAME"}` is a secret
//! reference: it survives into a `ResolvedFiring` and is fetched at spawn time,
//! so no secret value is ever written down. Both are a wire protocol shared by
//! frontends, the engine, validation and the step kinds — which is why they
//! live here and not in any one of them.

use serde_json::Value;

/// The marker a HIR `StepRef.config` uses for a value that is still an
/// expression. Lowering replaces these with concrete values before execution.
pub const EXPR_PLACEHOLDER_KEY: &str = "$expr";

/// The `Node::meta` key a frontend sets when it lowered one branch of a fork
/// into a graph of its own: `{ "fork": <node id in the caller's graph>,
/// "index": <branch ordinal> }`. Hosts read a node's branch role from the
/// graph shape; this key lets a branch that left its caller's graph keep the
/// role it would have had there. Shared by frontends and the driver, like the
/// placeholder keys.
pub const BRANCH_ROLE_META: &str = "branch_role";

/// The marker of an expansion item that stands for no item:
/// `{"$placeholder": true}`.
///
/// A `for_each` fan-out whose collector must fire even over an empty list
/// expands the list to this one item, so the template fires once and its
/// token reaches the collector. The clone it produces is a lowering
/// artifact, not a branch: a branch map gives its nodes no member role and
/// counts it in no fork, the event stream announces and closes the fork
/// with zero branches, and the step that receives the item does no work.
pub const PLACEHOLDER_ITEM_KEY: &str = "$placeholder";

/// The one expansion item an empty list expands to.
pub fn placeholder_item() -> Value {
    serde_json::json!({ PLACEHOLDER_ITEM_KEY: true })
}

/// Whether an expansion item is the placeholder for no item.
pub fn is_placeholder_item(item: &Value) -> bool {
    item.get(PLACEHOLDER_ITEM_KEY) == Some(&Value::Bool(true))
}

/// The marker for a secret reference: `{"$secret": "NAME"}`.
///
/// Unlike an expression placeholder, this one **survives** into a
/// `ResolvedFiring` and crosses the executor boundary. It has to: a
/// `ResolvedFiring` is serialized into the event log, and a resolved secret in
/// the log is a secret on disk. The value is fetched at spawn time instead,
/// straight into the child's environment.
///
/// Secrets are not in [`EvalEnv`](crate::EvalEnv) either, so a guard cannot
/// read one by construction.
pub const SECRET_REF_KEY: &str = "$secret";

/// A malformed secret reference: `{"$secret": <not a string>}`.
///
/// Returns the path of the first one found.
pub fn malformed_secret_ref(config: &Value) -> Option<String> {
    fn walk(value: &Value, path: &str) -> Option<String> {
        match value {
            Value::Object(map) => {
                if let Some(name) = map.get(SECRET_REF_KEY)
                    && !name.is_string()
                {
                    return Some(if path.is_empty() {
                        "<root>".into()
                    } else {
                        path.into()
                    });
                }
                map.iter().find_map(|(key, child)| {
                    let next = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(child, &next)
                })
            }
            Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(i, child)| walk(child, &format!("{path}[{i}]"))),
            _ => None,
        }
    }
    walk(config, "")
}

/// Where the first unresolved expression placeholder sits in a config, as a
/// dotted path (`""` when the whole config is one).
///
/// This is how "no unresolved `ExprId` crosses the executor boundary" is
/// enforced: `ResolvedFiring`'s constructor checks it at firing time.
pub fn placeholder_path(config: &Value) -> Option<String> {
    fn walk(value: &Value, path: &str) -> Option<String> {
        match value {
            Value::Object(map) => {
                if map.contains_key(EXPR_PLACEHOLDER_KEY) {
                    return Some(path.to_string());
                }
                map.iter().find_map(|(key, child)| {
                    let next = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(child, &next)
                })
            }
            Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(i, child)| walk(child, &format!("{path}[{i}]"))),
            _ => None,
        }
    }
    walk(config, "")
}

/// Whether a config still holds an unresolved expression placeholder.
pub fn contains_placeholder(config: &Value) -> bool {
    placeholder_path(config).is_some()
}

/// Rewrite every `{"$expr": id}` placeholder id through `f`, leaving the rest
/// of the config untouched. How the engine's splice remapper shifts
/// fragment-local expression ids into the live table — kept here so only this
/// module walks the placeholder encoding.
pub fn map_expr_ids(config: &Value, f: &impl Fn(u64) -> u64) -> Value {
    match config {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    if key == EXPR_PLACEHOLDER_KEY
                        && let Some(id) = child.as_u64()
                    {
                        (key.clone(), Value::from(f(id)))
                    } else {
                        (key.clone(), map_expr_ids(child, f))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|i| map_expr_ids(i, f)).collect()),
        other => other.clone(),
    }
}
