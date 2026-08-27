//! `github/action`: one phase of a JavaScript action.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use frontend_gha::action::{ActionSourceError, PinnedAction};
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

/// The runtime half of an action source. Manifest-only sources used by the
/// frontend do not need to invent a host tree.
pub trait ActionTreeSource: Send + Sync {
    fn tree(&self, pinned: &PinnedAction) -> Result<PathBuf, ActionSourceError>;
}

/// The action tree source, as a step finds it:
/// `ctx.require_capability::<ActionSourceCap>()`.
pub struct ActionSourceCap(pub Arc<dyn ActionTreeSource>);

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
    // The gate first: a phase whose condition is false stages nothing and spawns
    // nothing. See `RunStep` for the skipped/cancelled split.
    if let Some(gate) = &config.gate {
        match crate::gate::admitted(gate, &config.env, &ctx).await? {
            true => {}
            false if config.cancelled => return Ok(Outcome::cancelled()),
            false => return Ok(Outcome::skipped()),
        }
    }
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
    let (outcome, effects) = session.run(process, None, ctx, allow_unsecure).await;

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
    source: &Arc<dyn ActionTreeSource>,
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
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let producer = tokio::task::spawn_blocking(move || stream_files(&host_dir, tx));
    let mut writes = tokio::task::JoinSet::new();
    let action = pinned.to_string();
    while let Some((path, bytes)) = rx.recv().await {
        if writes.len() >= 8 {
            finish_write(&mut writes).await?;
        }
        let env = Arc::clone(&ctx.env);
        let destination = relative.join(&path);
        let action = action.clone();
        writes.spawn(async move {
            env.write_file(&destination, &bytes)
                .await
                .map_err(|e| StepFailure {
                    class: STAGE_CLASS,
                    message: format!("could not stage `{}` of `{action}`: {e}", path.display()),
                })
        });
    }
    while !writes.is_empty() {
        finish_write(&mut writes).await?;
    }
    producer
        .await
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("reading `{pinned}` did not complete: {e}"),
        })?
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("could not read the fetched `{pinned}`: {e}"),
        })?;
    ctx.env
        .write_file(&marker, b"")
        .await
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("could not mark `{pinned}` as staged: {e}"),
        })?;
    Ok(relative)
}

async fn finish_write(
    writes: &mut tokio::task::JoinSet<Result<(), StepFailure>>,
) -> Result<(), StepFailure> {
    writes
        .join_next()
        .await
        .expect("the write set is not empty")
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("staging an action file did not complete: {e}"),
        })?
}

/// Stream every regular file under `root`, by relative path, through a bounded
/// channel. The action size does not become one in-memory `Vec`.
fn stream_files(
    root: &Path,
    tx: tokio::sync::mpsc::Sender<(PathBuf, Vec<u8>)>,
) -> std::io::Result<()> {
    fn walk(
        dir: &Path,
        root: &Path,
        tx: &tokio::sync::mpsc::Sender<(PathBuf, Vec<u8>)>,
    ) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                walk(&path, root, tx)?;
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).expect("under root").to_path_buf();
                if tx.blocking_send((relative, std::fs::read(&path)?)).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
    walk(root, root, &tx)
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

    #[cfg(unix)]
    #[test]
    fn staging_does_not_follow_symbolic_links() {
        use std::os::unix::fs::symlink;

        let run = testkit::RunDir::new("action-staging-symlink");
        let root = run.path().join("action");
        std::fs::create_dir_all(&root).expect("action directory");
        std::fs::write(root.join("index.js"), "safe").expect("action file");
        std::fs::write(run.path().join("outside"), "secret").expect("outside file");
        symlink(run.path().join("outside"), root.join("linked")).expect("symbolic link");

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        stream_files(&root, tx).expect("stream action files");
        let files: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

        assert_eq!(files, vec![(PathBuf::from("index.js"), b"safe".to_vec())]);
    }
}
