//! GitHub Actions step kinds: what a `run:` step and a `uses:` step do at run time.
//!
//! Both are the core `process` step with GitHub's runner contract around it. A
//! [`RunStep`] runs the script with the `GITHUB_*` files in place and applies what
//! the script wrote to them; an [`ActionStep`] stages a fetched JavaScript action
//! and runs its entry point with `INPUT_*` set. Neither knows where the process
//! runs: they build a `ProcessConfig` and hand it to the process step, swapping the
//! log sender so `::` workflow commands are seen on the way past.
//!
//! The frontend (`frontend_gha`) lowers steps to these kinds and resolves actions to
//! commits at load time through an [`ActionSource`]; [`GitActionSource`] is the one
//! that fetches from git. The step finds the same source as a capability
//! ([`ActionSourceCap`]) to stage the tree at run time.

mod action;
pub mod commands;
pub mod config;
mod run;
pub mod session;
pub mod source;

pub use action::{ActionSourceCap, ActionStep};
pub use frontend_gha::action::{ActionRef, ActionSource, ActionSourceError, PinnedAction};
pub use frontend_gha::{ACTION_KIND, RUN_KIND, STATE_OUTPUT_KEY};
pub use run::RunStep;
pub use source::{GitActionSource, default_cache_dir};

use ir::Value;
use ir::placeholder::SECRET_REF_KEY;
use steps::StepFailure;

/// A `{"$secret": ...}` reference outside the maps that may hold one: it would
/// otherwise reach the process as literal JSON. Returns the offending path.
pub(crate) fn misplaced_secret(config: &Value, allowed_maps: &[&str]) -> Option<String> {
    fn is_secret_ref(value: &Value) -> bool {
        value
            .as_object()
            .is_some_and(|m| m.len() == 1 && m.contains_key(SECRET_REF_KEY))
    }
    fn walk(value: &Value, path: &str) -> Option<String> {
        match value {
            Value::Object(map) => {
                if map.contains_key(SECRET_REF_KEY) {
                    return Some(path.to_string());
                }
                map.iter()
                    .find_map(|(k, v)| walk(v, &format!("{path}.{k}")))
            }
            Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(i, v)| walk(v, &format!("{path}[{i}]"))),
            _ => None,
        }
    }
    let top = config.as_object()?;
    top.iter().find_map(|(key, value)| {
        if allowed_maps.contains(&key.as_str()) {
            let Some(map) = value.as_object() else {
                return walk(value, key);
            };
            map.iter().find_map(|(k, v)| {
                if is_secret_ref(v) {
                    None
                } else {
                    walk(v, &format!("{key}.{k}"))
                }
            })
        } else {
            walk(value, key)
        }
    })
}

pub(crate) fn secret_misplaced(path: String) -> StepFailure {
    StepFailure {
        class: steps::SECRET_MISPLACED_CLASS,
        message: format!(
            "`{SECRET_REF_KEY}` is only valid in `env` or `inputs`; found one at `{path}`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_fine_in_allowed_maps_and_nowhere_else() {
        let ok = json!({ "env": { "TOKEN": { "$secret": "T" } }, "inputs": { "token": { "$secret": "T" } } });
        assert_eq!(misplaced_secret(&ok, &["env", "inputs"]), None);
        let bad = json!({ "run": { "$secret": "T" } });
        assert_eq!(misplaced_secret(&bad, &["env"]), Some("run".into()));
        let nested = json!({ "env": { "X": { "nested": { "$secret": "T" } } } });
        assert_eq!(
            misplaced_secret(&nested, &["env"]),
            Some("env.X.nested".into())
        );
    }
}
