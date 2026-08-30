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
    REPO_DIR, RUNNER_DIR, Session, fold_into_outcome, shell_quote, stringify,
    unsecure_commands_allowed,
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
    // nothing.
    if let Some(outcome) =
        crate::gate::refusal(config.gate.as_ref(), config.cancelled, &config.env, &ctx).await?
    {
        return Ok(outcome);
    }
    let session = Session::begin(&ctx, &config.event).await?;

    // Where the action's files are, as the process sees them.
    let (action_dir, repository, git_ref) = match &config.action {
        ActionLocation::Pinned(pinned) => {
            let source = ctx.require_capability::<ActionSourceCap>()?;
            let root = stage(&ctx, &source.0, pinned).await?;
            let staged = match &pinned.reference.path {
                Some(path) => root.join(path.as_str()),
                None => root,
            };
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
    // The results backend, when the host stood one up — actions only, the
    // visibility GitHub gives the runtime token.
    if let Some(results) = ctx.capability::<crate::ResultsServiceCap>() {
        for (key, value) in results.env(ctx.env.host_address()) {
            env.insert(key, literal(value.to_string()));
        }
    }
    let allow_unsecure = unsecure_commands_allowed(&env, &*ctx.env);

    let entry = format!("{action_dir}/{}", config.entry.trim_start_matches("./"));
    let process = ProcessConfig {
        run: format!("{}exec node {}\n", session.prologue(), shell_quote(&entry)),
        // Bash, not sh: GitHub execs `node` directly with the full env, and a
        // dash/busybox `sh` would drop the hyphenated `INPUT_*` names the
        // toolkit contract requires (`INPUT_NODE-VERSION`) on the way through.
        shell: Shell::Bash,
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
/// return the staged repository root (relative to the workspace root).
pub(crate) async fn stage(
    ctx: &StepCtx,
    source: &Arc<dyn ActionTreeSource>,
    pinned: &PinnedAction,
) -> Result<PathBuf, StepFailure> {
    let reference = &pinned.reference;
    // The whole repository stages once per commit — a subpath action's entry
    // may reach beside its directory (`../lib/…`), exactly as on GitHub's
    // runners — and callers join what they need below the returned root: the
    // action's own directory, or a Dockerfile's parent.
    let root = PathBuf::from(RUNNER_DIR)
        .join("actions")
        .join(reference.owner.as_str())
        .join(reference.repo.as_str())
        .join(pinned.sha.as_str());
    let marker = root.join(".petri-staged");
    let already = ctx.env.read_file(&marker).await.map_err(|e| StepFailure {
        class: STAGE_CLASS,
        message: format!("could not check the staged action: {e}"),
    })?;
    if already.is_some() {
        return Ok(root);
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
    // One tarball, packed host-side from the fetched tree and extracted by the
    // environment's own `tar` — the delivery the checkout step uses. Mode bits
    // and symlinks survive, which per-file writes through `write_file` did not:
    // a shipped `setup.sh` arrived unexecutable and the action died spawning
    // it. One write also beats hundreds.
    let destination = root.to_string_lossy().into_owned();
    let tarball = tokio::task::spawn_blocking(move || pack_tree(&destination, &host_dir))
        .await
        .map_err(|e| stage_error(format!("packing `{pinned}` did not complete: {e}")))?
        .map_err(|e| stage_error(format!("could not pack `{pinned}`: {e}")))?;

    let tar_rel = root.with_extension("tar");
    ctx.env
        .write_file(&tar_rel, &tarball)
        .await
        .map_err(|e| stage_error(format!("could not write `{pinned}`'s archive: {e}")))?;
    drop(tarball);
    unpack(ctx, &tar_rel, pinned).await?;
    ctx.env
        .write_file(&marker, b"")
        .await
        .map_err(|e| StepFailure {
            class: STAGE_CLASS,
            message: format!("could not mark `{pinned}` as staged: {e}"),
        })?;
    Ok(root)
}

/// Pack the fetched tree as one archive whose entries live under
/// `destination`. Symlinks are archived as symlinks, never followed — a link
/// pointing outside the tree carries no target bytes, exactly as a git
/// checkout of the action would behave on GitHub's runners.
fn pack_tree(destination: &str, dir: &Path) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    builder
        .append_dir_all(destination, dir)
        .and_then(|()| builder.into_inner())
}

fn stage_error(message: String) -> StepFailure {
    StepFailure {
        class: STAGE_CLASS,
        message,
    }
}

/// Extract the staged archive with the environment's own `tar`. No cancel
/// ladder: action trees are small, `tar` always terminates, and the streamed
/// per-file writes this replaced never consulted cancellation either.
async fn unpack(ctx: &StepCtx, tar_rel: &Path, pinned: &PinnedAction) -> Result<(), StepFailure> {
    let workspace = ctx.env.workspace_path().to_string();
    let mut handle = ctx
        .env
        .spawn(executor::ProcessSpec {
            program: SmolStr::new("tar"),
            args: vec![
                SmolStr::new("-xf"),
                SmolStr::new(format!("{workspace}/{}", tar_rel.display())),
                SmolStr::new("-C"),
                SmolStr::new(&workspace),
            ],
            env: Default::default(),
            cwd: None,
        })
        .await
        .map_err(|e| stage_error(format!("could not run `tar` for `{pinned}`: {e}")))?;
    // Silent on success; on failure the last lines are the diagnosis.
    let mut said = std::collections::VecDeque::with_capacity(4);
    if let Some(mut lines) = handle.lines() {
        while let Some(line) = lines.recv().await {
            if said.len() == 4 {
                said.pop_front();
            }
            said.push_back(line.line);
        }
    }
    let status = handle
        .wait()
        .await
        .map_err(|e| stage_error(format!("extracting `{pinned}` did not complete: {e}")))?;
    if status.code != Some(0) {
        return Err(stage_error(format!(
            "could not extract `{pinned}`: tar said {}",
            said.into_iter().collect::<Vec<_>>().join(" | "),
        )));
    }
    Ok(())
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
    fn staging_archives_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let run = testkit::RunDir::new("action-staging-symlink");
        let root = run.path().join("action");
        std::fs::create_dir_all(&root).expect("action directory");
        std::fs::write(root.join("index.js"), "safe").expect("action file");
        std::fs::write(run.path().join("outside"), "secret-bytes").expect("outside file");
        symlink(run.path().join("outside"), root.join("linked")).expect("symbolic link");

        let bytes = pack_tree("staged", &root).expect("pack");
        let mut saw_link = false;
        for entry in tar::Archive::new(&bytes[..]).entries().expect("entries") {
            let entry = entry.expect("entry");
            if entry.path().expect("path").ends_with("linked") {
                saw_link = true;
                assert!(entry.header().entry_type().is_symlink());
                assert_eq!(entry.header().size().expect("size"), 0);
            }
        }
        assert!(saw_link, "the link travels as a link");
        // The link's target never enters the archive: following it would let a
        // hostile action tree stage bytes from outside itself.
        assert!(!bytes.windows(12).any(|w| w == b"secret-bytes"));
    }
}
