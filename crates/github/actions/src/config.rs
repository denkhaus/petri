//! The step configs the GitHub Actions frontend emits.
//!
//! The frontend writes these as JSON with `$expr` placeholders; by the time a step
//! deserializes one they are resolved, and the only non-literal left is a
//! `{"$secret": NAME}` reference in an env-shaped position.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ir::Value;
use serde::Deserialize;
use smol_str::SmolStr;
use steps::{Shell, SoftFail, ValueOrSecretRef};

pub use frontend_gha::action::{ActionLocation, Phase};

/// `github/run`: a `run:` step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub run: String,
    #[serde(default)]
    pub shell: Shell,
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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionConfig {
    pub action: ActionLocation,
    pub phase: Phase,
    /// `runs.main`, `runs.pre` or `runs.post`, relative to the action directory.
    pub entry: String,
    /// `runs.using`: `node20`, `node24`, … Every version runs on the `node` found on
    /// `PATH` in the job environment.
    #[serde(default)]
    pub runtime: String,
    /// Declared inputs with the caller's values or their defaults, plus undeclared
    /// `with:` keys. Each becomes `INPUT_<NAME>`.
    #[serde(default)]
    pub inputs: BTreeMap<String, ValueOrSecretRef>,
    #[serde(default)]
    pub env: BTreeMap<SmolStr, ValueOrSecretRef>,
    /// State an earlier phase of this action saved; each entry becomes `STATE_<name>`.
    #[serde(default)]
    pub state: BTreeMap<String, Value>,
    #[serde(default)]
    pub soft_fail: SoftFail,
    #[serde(default)]
    pub event: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_action_config_deserializes_both_locations() {
        let pinned: ActionConfig = serde_json::from_value(json!({
            "action": { "reference": { "owner": "actions", "repo": "checkout", "ref": "v4" }, "sha": "abc" },
            "phase": "main",
            "entry": "dist/index.js",
            "inputs": { "token": { "$secret": "GITHUB_TOKEN" }, "fetch-depth": 1 },
        }))
        .expect("deserializes");
        assert!(matches!(pinned.action, ActionLocation::Pinned(_)));
        assert_eq!(pinned.phase, Phase::Main);

        let local: ActionConfig = serde_json::from_value(json!({
            "action": { "local": ".github/actions/hello" },
            "phase": "post",
            "entry": "post.js",
        }))
        .expect("deserializes");
        assert!(matches!(local.action, ActionLocation::Local { .. }));
    }
}
