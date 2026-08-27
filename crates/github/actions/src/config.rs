//! The step configs the GitHub Actions frontend emits.
//!
//! The frontend writes these as JSON with `$expr` placeholders; by the time a step
//! deserializes one they are resolved, and the only non-literal left is a
//! `{"$secret": NAME}` reference in an env-shaped position.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ir::Value;
use serde::{Deserialize, Deserializer, de};
use smol_str::SmolStr;
use steps::{ProcessConfig, Shell, SoftFail, ValueOrSecretRef};

pub use frontend_gha::action::ActionLocation;
use frontend_gha::action::validate_relative_action_path;

/// `github/run`: a `run:` step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub run: String,
    /// The step's condition, as the frontend's gate tree. The step evaluates it
    /// before session files are created and before any process spawns; false
    /// means `Outcome::skipped()` — or a cancelled outcome when `cancelled` is
    /// set. Absent means run.
    #[serde(default)]
    pub gate: Option<Value>,
    /// The engine's `scope_cancelled` static at firing time: a false gate then
    /// records `Cancelled`, as GitHub reports post-cancel non-cleanup steps.
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub shell: Shell,
    /// A custom shell template (`bash -el {0}`, `python {0}`): the step writes the
    /// script to a file and substitutes its path for `{0}`, as GitHub does. When
    /// set, `shell` is not used.
    #[serde(default)]
    pub shell_command: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<SmolStr, ValueOrSecretRef>,
    /// `working-directory`, relative to `GITHUB_WORKSPACE`.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub soft_fail: SoftFail,
    /// `github.event`, written to the file `GITHUB_EVENT_PATH` names.
    #[serde(default)]
    pub event: Value,
}

/// `github/action`: one phase of a JavaScript action.
#[derive(Debug)]
pub struct ActionConfig {
    pub action: ActionLocation,
    /// `runs.main`, `runs.pre` or `runs.post`, relative to the action directory.
    pub entry: String,
    /// The phase's condition (`if:`, `pre-if`, `post-if`) as the frontend's gate
    /// tree, evaluated before anything is staged or spawned. Absent means run.
    pub gate: Option<Value>,
    /// The engine's `scope_cancelled` static at firing time; see `RunConfig`.
    pub cancelled: bool,
    /// Declared inputs with the caller's values or their defaults, plus undeclared
    /// `with:` keys. Each becomes `INPUT_<NAME>`.
    pub inputs: BTreeMap<String, ValueOrSecretRef>,
    pub env: BTreeMap<SmolStr, ValueOrSecretRef>,
    /// State an earlier phase of this action saved; each entry becomes `STATE_<name>`.
    pub state: BTreeMap<String, Value>,
    pub soft_fail: SoftFail,
    pub event: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActionConfig {
    action: ActionLocation,
    entry: String,
    #[serde(default)]
    gate: Option<Value>,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    inputs: BTreeMap<String, ValueOrSecretRef>,
    #[serde(default)]
    env: BTreeMap<SmolStr, ValueOrSecretRef>,
    #[serde(default)]
    state: BTreeMap<String, Value>,
    #[serde(default)]
    soft_fail: SoftFail,
    #[serde(default)]
    event: Value,
}

impl<'de> Deserialize<'de> for ActionConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawActionConfig::deserialize(deserializer)?;
        match &raw.action {
            ActionLocation::Pinned(pinned) => pinned.validate().map_err(de::Error::custom)?,
            ActionLocation::Local { local } => {
                validate_relative_action_path(local, true).map_err(de::Error::custom)?
            }
        }
        validate_relative_action_path(&raw.entry, false).map_err(de::Error::custom)?;
        Ok(Self {
            action: raw.action,
            entry: raw.entry,
            gate: raw.gate,
            cancelled: raw.cancelled,
            inputs: raw.inputs,
            env: raw.env,
            state: raw.state,
            soft_fail: raw.soft_fail,
            event: raw.event,
        })
    }
}

/// Every string in a process config that can carry a lowered GitHub placeholder.
pub(crate) fn process_texts(process: &ProcessConfig) -> impl Iterator<Item = &str> {
    std::iter::once(process.run.as_str()).chain(process.env.values().filter_map(
        |value| match value {
            ValueOrSecretRef::Literal(Value::String(text)) => Some(text.as_str()),
            _ => None,
        },
    ))
}

/// Replace selected text-bearing fields without duplicating the field walk in
/// each placeholder resolver. `None` means the field is unchanged.
pub(crate) fn try_map_process_texts<E>(
    process: &mut ProcessConfig,
    mut map: impl FnMut(&str) -> Result<Option<String>, E>,
) -> Result<(), E> {
    if let Some(text) = map(&process.run)? {
        process.run = text;
    }
    for value in process.env.values_mut() {
        if let ValueOrSecretRef::Literal(Value::String(text)) = value
            && let Some(replacement) = map(text)?
        {
            *text = replacement;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_action_config_deserializes_both_locations() {
        let pinned: ActionConfig = serde_json::from_value(json!({
            "action": { "reference": { "owner": "actions", "repo": "checkout", "ref": "v4" }, "sha": "0123456789abcdef0123456789abcdef01234567" },
            "entry": "dist/index.js",
            "inputs": { "token": { "$secret": "GITHUB_TOKEN" }, "fetch-depth": 1 },
        }))
        .expect("deserializes");
        assert!(matches!(pinned.action, ActionLocation::Pinned(_)));

        let local: ActionConfig = serde_json::from_value(json!({
            "action": { "local": ".github/actions/hello" },
            "entry": "post.js",
        }))
        .expect("deserializes");
        assert!(matches!(local.action, ActionLocation::Local { .. }));

        assert!(
            serde_json::from_value::<ActionConfig>(json!({
                "action": { "local": "../outside" },
                "entry": "index.js",
            }))
            .is_err()
        );
    }
}
