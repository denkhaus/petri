//! The step configs the GitHub Actions frontend emits.
//!
//! The frontend writes these as JSON with `$expr` placeholders; by the time a
//! step deserializes one they are resolved, and the only non-literal left is a
//! `{"$secret": NAME}` reference in an env-shaped position.

use std::collections::BTreeMap;
use std::iter;
use std::path::PathBuf;

pub use frontend_gha::action::ActionLocation;
use frontend_gha::action::{resolve_manifest_path, validate_relative_action_path};
use ir::Value;
use serde::{Deserialize, Deserializer, de};
use smol_str::SmolStr;
use steps::{ProcessConfig, Shell, SoftFail, ValueOrSecretRef};

/// Preparation required before a shell template runs its script file.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellScript {
    #[default]
    Plain,
    /// GitHub's built-in `pwsh` script contract.
    PowerShell,
}

/// `github/run`: a `run:` step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub run:           String,
    /// The step's condition, as the frontend's gate tree. The step evaluates it
    /// before session files are created and before any process spawns; false
    /// means `Outcome::skipped()` — or a cancelled outcome when `cancelled` is
    /// set. Absent means run.
    #[serde(default)]
    pub gate:          Option<Value>,
    /// The engine's `scope_cancelled` static at firing time: a false gate then
    /// records `Cancelled`, as GitHub reports post-cancel non-cleanup steps.
    #[serde(default)]
    pub cancelled:     bool,
    #[serde(default)]
    pub shell:         Shell,
    /// A custom shell template (`bash -el {0}`, `python {0}`): the step writes
    /// the script to a file and substitutes its path for `{0}`, as GitHub
    /// does. When set, `shell` is not used.
    #[serde(default)]
    pub shell_command: Option<String>,
    /// Preparation for a built-in shell whose script contract is more than its
    /// command template. Custom shell templates leave this as `plain`.
    #[serde(default)]
    pub shell_script:  ShellScript,
    #[serde(default)]
    pub env:           BTreeMap<SmolStr, ValueOrSecretRef>,
    /// `working-directory`, relative to `GITHUB_WORKSPACE`.
    #[serde(default)]
    pub working_dir:   Option<PathBuf>,
    #[serde(default)]
    pub soft_fail:     SoftFail,
    /// `github.event`, written to the file `GITHUB_EVENT_PATH` names.
    #[serde(default)]
    pub event:         Value,
}

/// `github/checkout`: the local-checkout substitute — the workspace
/// materializes from the run's own repository.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutConfig {
    /// The repository's host path, from the `petri.repo` run parameter. A run
    /// whose host filled nothing resolves to null — the step fails routably.
    #[serde(default)]
    pub source:     Option<String>,
    /// The `path:` input: destination relative to `GITHUB_WORKSPACE`.
    #[serde(default)]
    pub path:       Option<String>,
    /// The run's fully-qualified ref (`github.ref`): a `refs/heads/*` value
    /// names the branch the snapshot leaves checked out, as GitHub's checkout
    /// does; anything else stays as the clone landed (detached).
    #[serde(default, rename = "ref")]
    pub reference:  Option<String>,
    /// `github.repository` — with `server_url`, the URL the snapshot's
    /// `origin` remote is set to, as GitHub's checkout configures it.
    #[serde(default)]
    pub repository: Option<String>,
    /// `github.server_url`.
    #[serde(default)]
    pub server_url: Option<String>,
    /// The step's condition, as the frontend's gate tree; see [`RunConfig`].
    #[serde(default)]
    pub gate:       Option<Value>,
    #[serde(default)]
    pub cancelled:  bool,
    #[serde(default)]
    pub soft_fail:  SoftFail,
}

/// `github/action`: one phase of a JavaScript action.
#[derive(Debug)]
pub struct ActionConfig {
    pub action:    ActionLocation,
    /// `runs.main`, `runs.pre` or `runs.post`, relative to the action
    /// directory.
    pub entry:     String,
    /// The phase's condition (`if:`, `pre-if`, `post-if`) as the frontend's
    /// gate tree, evaluated before anything is staged or spawned. Absent
    /// means run.
    pub gate:      Option<Value>,
    /// The engine's `scope_cancelled` static at firing time; see `RunConfig`.
    pub cancelled: bool,
    /// Declared inputs with the caller's values or their defaults, plus
    /// undeclared `with:` keys. Each becomes `INPUT_<NAME>`.
    pub inputs:    BTreeMap<String, ValueOrSecretRef>,
    pub env:       BTreeMap<SmolStr, ValueOrSecretRef>,
    /// State an earlier phase of this action saved; each entry becomes
    /// `STATE_<name>`.
    pub state:     BTreeMap<String, Value>,
    pub soft_fail: SoftFail,
    pub event:     Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActionConfig {
    action:    ActionLocation,
    entry:     String,
    #[serde(default)]
    gate:      Option<Value>,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    inputs:    BTreeMap<String, ValueOrSecretRef>,
    #[serde(default)]
    env:       BTreeMap<SmolStr, ValueOrSecretRef>,
    #[serde(default)]
    state:     BTreeMap<String, Value>,
    #[serde(default)]
    soft_fail: SoftFail,
    #[serde(default)]
    event:     Value,
}

/// The manifest's entry (`runs.main`/`pre`/`post`), validated the way the
/// runner joins it: relative to the action directory, `..` allowed only as far
/// as the fetched repository root — the checkout root (`GITHUB_WORKSPACE`) for
/// a local action ([`resolve_manifest_path`]). Nothing may climb past that
/// root: the workspace beyond it is the runner's, and on a host job the
/// filesystem beyond *that* is the machine's. Only the bound matters here; the
/// step joins the entry as written.
fn validate_entry(action: &ActionLocation, entry: &str) -> Result<(), String> {
    if entry.is_empty() {
        return Err("the action has no entry point".to_string());
    }
    resolve_manifest_path(action.directory(), entry)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

impl<'de> Deserialize<'de> for ActionConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawActionConfig::deserialize(deserializer)?;
        match &raw.action {
            ActionLocation::Pinned(pinned) => pinned.validate().map_err(de::Error::custom)?,
            ActionLocation::Local { local } => {
                validate_relative_action_path(local, true).map_err(de::Error::custom)?;
            }
        }
        validate_entry(&raw.action, &raw.entry).map_err(de::Error::custom)?;
        Ok(Self {
            action:    raw.action,
            entry:     raw.entry,
            gate:      raw.gate,
            cancelled: raw.cancelled,
            inputs:    raw.inputs,
            env:       raw.env,
            state:     raw.state,
            soft_fail: raw.soft_fail,
            event:     raw.event,
        })
    }
}

/// `github/docker_action`: one phase of a Docker container action. One
/// container per phase invocation, run through the scope's container runner.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerActionConfig {
    pub image:      DockerActionImage,
    /// The phase's entrypoint (`runs.entrypoint`, a `with.entrypoint`, or the
    /// phase's `pre-entrypoint`/`post-entrypoint`); the image's own when
    /// absent.
    #[serde(default)]
    pub entrypoint: Option<Value>,
    /// `runs.args`, one argument per entry (main phase only).
    #[serde(default)]
    pub args:       Vec<Value>,
    /// A `uses: docker://` step's `with.args`: one string, shell-split after
    /// expressions and secrets resolve, as GitHub does.
    #[serde(default)]
    pub args_text:  Option<Value>,
    /// The phase's condition as the frontend's gate tree; see [`RunConfig`].
    #[serde(default)]
    pub gate:       Option<Value>,
    /// The engine's `scope_cancelled` static at firing time; see [`RunConfig`].
    #[serde(default)]
    pub cancelled:  bool,
    /// Declared inputs with the caller's values or their defaults, plus
    /// undeclared `with:` keys. Each becomes `INPUT_<NAME>`.
    #[serde(default)]
    pub inputs:     BTreeMap<String, Value>,
    #[serde(default)]
    pub env:        BTreeMap<SmolStr, ValueOrSecretRef>,
    /// State an earlier phase of this action saved; each entry becomes
    /// `STATE_<name>`.
    #[serde(default)]
    pub state:      BTreeMap<String, Value>,
    #[serde(default)]
    pub soft_fail:  SoftFail,
    #[serde(default)]
    pub event:      Value,
}

/// Where a Docker action's image comes from.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DockerActionImage {
    /// A registry image (`docker://…`, prefix stripped).
    Registry(String),
    /// Built from the action's own Dockerfile.
    Dockerfile(DockerfileImage),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerfileImage {
    /// Where the action's tree is — the build context.
    pub action: ActionLocation,
    /// The Dockerfile, relative to the action (`runs.image` as written).
    pub file:   String,
}

/// Every string in a process config that can carry a lowered GitHub
/// placeholder.
pub(crate) fn process_texts(process: &ProcessConfig) -> impl Iterator<Item = &str> {
    iter::once(process.run.as_str()).chain(process.env.values().filter_map(|value| match value {
        ValueOrSecretRef::Literal(Value::String(text)) => Some(text.as_str()),
        _ => None,
    }))
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
    use serde_json::json;

    use super::*;

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

#[cfg(test)]
mod entry_tests {
    use frontend_gha::action::{ActionRef, PinnedAction};
    use smol_str::SmolStr;

    use super::*;

    fn pinned(reference: &str) -> ActionLocation {
        ActionLocation::Pinned(PinnedAction {
            reference: ActionRef::parse(reference).expect("valid"),
            sha:       SmolStr::new("0123456789012345678901234567890123456789"),
        })
    }

    #[test]
    fn entries_resolve_within_the_repository_root() {
        let root_action = pinned("octo/tool@v1");
        assert!(validate_entry(&root_action, "dist/index.js").is_ok());
        assert!(
            validate_entry(&root_action, "./dist/main.js").is_ok(),
            "a leading ./ is normal"
        );
        assert!(
            validate_entry(&root_action, "../outside.js").is_err(),
            "a root action cannot climb"
        );

        let subpath = pinned("github/codeql-action/init@v3");
        assert!(
            validate_entry(&subpath, "../lib/init-entry.js").is_ok(),
            "a subpath action may reach beside itself"
        );
        assert!(
            validate_entry(&subpath, "../../escape.js").is_err(),
            "but never past the repository root"
        );
        assert!(
            validate_entry(&subpath, "a/../../../escape.js").is_err(),
            "descending first buys no extra climb"
        );

        let local = ActionLocation::Local {
            local: "tools/act".to_string(),
        };
        assert!(validate_entry(&local, "../shared/run.js").is_ok());
        assert!(validate_entry(&local, "../../../outside.js").is_err());
        assert!(validate_entry(&root_action, "/abs.js").is_err());
        assert!(validate_entry(&root_action, "").is_err());
    }
}
