//! The two markers a step config may carry across a boundary.
//!
//! `{"$expr": id}` is a HIR placeholder: lowering leaves one where a value is still
//! an expression, and the engine resolves it against the firing's environment before
//! the step runs. `{"$secret": "NAME"}` is a secret reference: it survives into a
//! `ResolvedFiring` and is fetched at spawn time, so no secret value is ever written
//! down. Both are a wire protocol shared by frontends, the engine, validation and the
//! step kinds — which is why they live here and not in any one of them.

use serde_json::Value;

/// The marker a HIR `StepRef.config` uses for a value that is still an expression.
/// Lowering replaces these with concrete values before execution.
pub const EXPR_PLACEHOLDER_KEY: &str = "$expr";

/// The marker for a secret reference: `{"$secret": "NAME"}`.
///
/// Unlike an expression placeholder, this one **survives** into a `ResolvedFiring`
/// and crosses the executor boundary. It has to: a `ResolvedFiring` is serialized
/// into the event log, and a resolved secret in the log is a secret on disk. The
/// value is fetched at spawn time instead, straight into the child's environment.
///
/// Secrets are not in [`EvalEnv`](crate::EvalEnv) either, so a guard cannot read one
/// by construction.
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

/// Where the first unresolved expression placeholder sits in a config, as a dotted
/// path (`""` when the whole config is one).
///
/// Both halves of "no unresolved `ExprId` crosses the executor boundary" use this:
/// [`validate_plan`](crate::validate::validate_plan) at load time, and `ResolvedFiring`'s constructor at firing time.
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
