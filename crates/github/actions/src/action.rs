//! `github/action`: one phase of a JavaScript action.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use frontend_gha::action::{ActionSource, PinnedAction};
use ir::{Outcome, Value};
use serde_json::Map;
use smol_str::SmolStr;
use steps::{ProcessConfig, Shell, Step, StepCtx, StepFailure, ValueOrSecretRef};

use crate::config::{ActionConfig, ActionLocation};
use crate::session::{
    REPO_DIR, RUNNER_DIR, Session, env_truthy, fold_into_outcome, shell_quote, stringify,
};

/// The action could not be fetched.
pub const FETCH_CLASS: &str = "action_fetch";
/// The action's files could not be put into the job environment.
pub const STAGE_CLASS: &str = "action_stage";

/// The action source, as a step finds it: `ctx.require_capability::<ActionSourceCap>()`.
pub struct ActionSourceCap(pub Arc<dyn ActionSource>);

/// Runs `node <entry>` of a staged action with the runner contract in place.
pub struct ActionStep;

#[async_trait::async_trait]
impl Step for ActionStep {
    const NAME: &'static str = frontend_gha::ACTION_KIND;
    type Config = ActionConfig;

    fn check_raw(&self, config: &Value) -> Result<(), StepFailure> {
        steps::check_misplaced_secret(config, &["env", "inputs"])
    }

    async fn run(&self, config: ActionConfig, ctx: StepCtx) -> Outcome {
        match execute(config, ctx).await {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }
}

async fn execute(config: ActionConfig, ctx: StepCtx) -> Result<Outcome, StepFailure> {
    let session = Session::begin(&ctx, &config.event).await?;

    // Where the action's files are, as the process sees them.
    let (action_dir, repository, git_ref) = match &config.action {
        ActionLocation::Pinned(pinned) => {
            let source = ctx.require_capability::<ActionSourceCap>()?;
            let staged = stage(&ctx, &source.0, pinned).await?;
            (
                format!("{}/{}", session.workspace(), staged.display()),
                pinned.reference.repository(),
                pinned.reference.git_ref.to_string(),
            )
        }
        ActionLocation::Local { local } => (
            format!("{}/{}", session.github_workspace(), local.trim_matches('/')),
            String::new(),
            String::new(),
        ),
    };

    let mut env = config.env;
    for (name, value) in &config.inputs {
        env.insert(SmolStr::new(input_variable(name)), value.clone());
    }
    for (name, value) in &config.state {
        env.insert(
            SmolStr::new(format!("STATE_{name}")),
            ValueOrSecretRef::Literal(Value::String(stringify(value))),
        );
    }
    for (key, value) in session.env(&ctx.node) {
        env.entry(key)
            .or_insert_with(|| ValueOrSecretRef::Literal(Value::String(value.to_string())));
    }
    let literal = |s: String| ValueOrSecretRef::Literal(Value::String(s));
    env.insert(
        SmolStr::new("GITHUB_ACTION_PATH"),
        literal(action_dir.clone()),
    );
    env.insert(
        SmolStr::new("GITHUB_ACTION_REPOSITORY"),
        literal(repository),
    );
    env.insert(SmolStr::new("GITHUB_ACTION_REF"), literal(git_ref));
    let allow_unsecure = env_truthy(&env, "ACTIONS_ALLOW_UNSECURE_COMMANDS");

    let entry = format!("{action_dir}/{}", config.entry.trim_start_matches("./"));
    let process = ProcessConfig {
        run: format!("{}exec node {}\n", session.prologue(), shell_quote(&entry)),
        shell: Shell::Sh,
        env,
        working_dir: Some(PathBuf::from(REPO_DIR)),
        soft_fail: config.soft_fail,
        output_env_aliases: vec![SmolStr::new("GITHUB_OUTPUT")],
    };
    let (outcome, effects) = session.run(process, ctx, allow_unsecure).await;

    // State accumulates across phases: what this phase inherited plus what it saved.
    let mut state: Map<String, Value> = config
        .state
        .into_iter()
        .map(|(k, v)| (k, Value::String(stringify(&v))))
        .collect();
    state.extend(effects.state.clone());
    Ok(fold_into_outcome(outcome, effects, state))
}

/// `INPUT_<NAME>`: uppercased, spaces to underscores, as the toolkit reads it.
pub fn input_variable(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_uppercase())
}

/// Put the action's tree into the job environment, once per scope instance, and
/// return where it went (relative to the workspace root).
async fn stage(
    ctx: &StepCtx,
    source: &Arc<dyn ActionSource>,
    pinned: &PinnedAction,
) -> Result<PathBuf, StepFailure> {
    let reference = &pinned.reference;
    let mut relative = PathBuf::from(RUNNER_DIR)
        .join("actions")
        .join(reference.owner.as_str())
        .join(reference.repo.as_str())
        .join(pinned.sha.as_str());
    if let Some(path) = &reference.path {
        relative = relative.join(path.as_str());
    }
    let marker = relative.join(".petri-staged");
    let already = ctx.env.read_file(&marker).await.map_err(|e| StepFailure {
        class: STAGE_CLASS,
        message: format!("could not check the staged action: {e}"),
    })?;
    if already.is_some() {
        return Ok(relative);
    }

    let source = Arc::clone(source);
    let pinned_owned = pinned.clone();
    let host_dir = tokio::task::spawn_blocking(move || source.tree(&pinned_owned))
        .await
        .map_err(|e| StepFailure {
            class: FETCH_CLASS,
            message: format!("fetching `{pinned}` did not complete: {e}"),
        })?
        .map_err(|e| StepFailure {
            class: FETCH_CLASS,
            message: e.to_string(),
        })?;
    let files = tokio::task::spawn_blocking(move || collect_files(&host_dir))
        .await
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("reading `{pinned}` did not complete: {e}"),
        })?
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("could not read the fetched `{pinned}`: {e}"),
        })?;
    for (path, bytes) in files {
        ctx.env
            .write_file(&relative.join(&path), &bytes)
            .await
            .map_err(|e| StepFailure {
                class: STAGE_CLASS,
                message: format!("could not stage `{}` of `{pinned}`: {e}", path.display()),
            })?;
    }
    ctx.env
        .write_file(&marker, b"")
        .await
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("could not mark `{pinned}` as staged: {e}"),
        })?;
    Ok(relative)
}

/// Every regular file under `root`, by path relative to it, `.git` excluded.
fn collect_files(root: &Path) -> std::io::Result<Vec<(PathBuf, Vec<u8>)>> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(&path, root, out)?;
            } else if path.is_file() {
                let relative = path.strip_prefix(root).expect("under root").to_path_buf();
                out.push((relative, std::fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_names_become_variables_the_toolkit_reads() {
        assert_eq!(input_variable("node-version"), "INPUT_NODE-VERSION");
        assert_eq!(input_variable("fetch depth"), "INPUT_FETCH_DEPTH");
        assert_eq!(input_variable("Token"), "INPUT_TOKEN");
    }
}
