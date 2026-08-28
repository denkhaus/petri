//! `github/checkout`: the local-checkout substitute.
//!
//! A local runner should test *the tree you have*. Where the lowering
//! substituted a supportable `actions/checkout` call, this step materializes
//! `GITHUB_WORKSPACE` from the run's own repository — the committed HEAD as a
//! depth-1 local clone, plus the uncommitted tracked diff and the
//! untracked-but-not-ignored files, never the ignored bulk (`target/`,
//! `node_modules/`). `.git` rides along, so later `git` steps keep working.
//! Offline, token-less, and an explicit documented delta from GitHub.
//!
//! The workspace stays a copy: the snapshot is assembled host-side in a
//! scratch directory (the real repository is never bind-mounted and never
//! mutated), packed as one tarball — mode bits and symlinks survive — written
//! through `ExecEnv::write_file` like every other runner file, and extracted
//! by the environment's own `tar`. Host and container scopes get the workspace
//! identically.
//!
//! A repository without `.git` — the corpus's fetched workflow trees — copies
//! as a plain tree: no history to clone, nothing ignored to skip.

use std::path::{Path, PathBuf};

use ir::{LogStream, Outcome, Value};
use serde_json::Map;
use smol_str::SmolStr;
use steps::{Ending, Step, StepCtx, StepFailure, ending_outcome, ladder};

use crate::config::CheckoutConfig;
use crate::session::REPO_DIR;

/// The failure class for everything checkout-shaped: a missing source, a git
/// that cannot snapshot, an extraction that failed.
pub const CHECKOUT_CLASS: &str = "checkout";

/// The step kind. `NAME` must match what the frontend emits.
pub struct CheckoutStep;

#[async_trait::async_trait]
impl Step for CheckoutStep {
    const NAME: &'static str = frontend_gha::CHECKOUT_KIND;
    type Config = CheckoutConfig;

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome {
        match execute(config, ctx).await {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }
}

async fn execute(config: CheckoutConfig, mut ctx: StepCtx) -> Result<Outcome, StepFailure> {
    let no_env = std::collections::BTreeMap::new();
    if let Some(outcome) =
        crate::gate::refusal(config.gate.as_ref(), config.cancelled, &no_env, &ctx).await?
    {
        return Ok(outcome);
    }
    let Some(source) = config.source.as_deref().filter(|s| !s.is_empty()) else {
        return Err(StepFailure {
            class: CHECKOUT_CLASS,
            message: "no local repository is configured for this run — the host fills the \
                      `petri.repo` run parameter from the workflow's repository root"
                .into(),
        });
    };
    let source = PathBuf::from(source);

    // The snapshot, assembled beside nothing the run owns and removed on the
    // way out whatever happens.
    let scratch = std::env::temp_dir().join(format!(
        "petri-checkout-{}-{}",
        std::process::id(),
        ctx.firing.raw()
    ));
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    let result = materialize(&source, &scratch, &config, &mut ctx).await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    result
}

async fn materialize(
    source: &Path,
    scratch: &Path,
    config: &CheckoutConfig,
    ctx: &mut StepCtx,
) -> Result<Outcome, StepFailure> {
    let clone = scratch.join("clone");
    tokio::fs::create_dir_all(&clone)
        .await
        .map_err(|e| checkout_error(format!("could not create a scratch dir: {e}")))?;

    let commit = if source.join(".git").exists() {
        snapshot_repository(source, &clone, ctx).await?
    } else {
        // No history: a plain tree (the corpus case) copies wholesale.
        copy_tree(source, &clone)
            .map_err(|e| checkout_error(format!("could not copy `{}`: {e}", source.display())))?;
        String::new()
    };

    // One tarball, entries already prefixed with their workspace-relative
    // destination, extracted at the workspace root by the environment's own
    // `tar` — which is what preserves mode bits inside a container.
    let destination = match config.path.as_deref() {
        Some(path) => format!("{REPO_DIR}/{}", path.trim_matches('/')),
        None => REPO_DIR.to_string(),
    };
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    let tarball = builder
        .append_dir_all(&destination, &clone)
        .and_then(|()| builder.into_inner())
        .map_err(|e| checkout_error(format!("could not pack the snapshot: {e}")))?;

    let tar_rel = format!(".ci/checkout/{}.tar", ctx.firing.raw());
    ctx.env
        .write_file(Path::new(&tar_rel), &tarball)
        .await
        .map_err(|e| checkout_error(format!("could not write the snapshot: {e}")))?;
    drop(tarball);

    let ending = extract(&tar_rel, ctx).await?;
    let mut output = Map::new();
    output.insert("commit".into(), Value::String(commit));
    Ok(ending_outcome(ending, &config.soft_fail, output))
}

/// The committed HEAD plus the working tree's uncommitted state, as a fresh
/// depth-1 clone overlaid with every path `git status` names.
async fn snapshot_repository(
    source: &Path,
    clone: &Path,
    ctx: &mut StepCtx,
) -> Result<String, StepFailure> {
    let url = format!("file://{}", source.display());
    git(
        None,
        &[
            "clone",
            "--depth",
            "1",
            "--quiet",
            "--",
            &url,
            &clone.display().to_string(),
        ],
    )
    .await?;

    // The dirty overlay: worktree truth wins, path by path.
    let porcelain = git(
        Some(source),
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )
    .await?;
    let mut copied = 0usize;
    for entry in porcelain_paths(porcelain.as_bytes()) {
        let from = source.join(&entry);
        let to = clone.join(&entry);
        if from.symlink_metadata().is_ok() {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| checkout_error(format!("overlay `{entry}`: {e}")))?;
            }
            let _ = std::fs::remove_file(&to);
            copy_entry(&from, &to)
                .map_err(|e| checkout_error(format!("overlay `{entry}`: {e}")))?;
            copied += 1;
        } else {
            // Deleted in the working tree: deleted in the snapshot.
            let _ = std::fs::remove_file(&to);
        }
    }
    if copied > 0 {
        ctx.log(
            LogStream::Stdout,
            format!("local checkout: committed HEAD plus {copied} uncommitted path(s)"),
        )
        .await;
    }

    let head = git(Some(clone), &["rev-parse", "HEAD"]).await?;
    Ok(head.trim().to_string())
}

/// Paths out of `git status --porcelain -z`: `XY path\0`, with a rename's
/// original consumed alongside so it never reads as its own entry.
fn porcelain_paths(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = bytes.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        let text = String::from_utf8_lossy(field);
        let Some((status, path)) = text.split_at_checked(3) else {
            continue;
        };
        if status.starts_with('R') || status.starts_with('C') {
            let _original = fields.next();
        }
        out.push(path.to_string());
    }
    out
}

fn copy_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = from.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to)?;
        return Ok(());
    }
    std::fs::copy(from, to).map(|_| ())
}

/// A plain tree, copied whole (skipping nothing: with no git there is no
/// ignore file semantics to honor).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            copy_entry(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Run the environment's `tar` over the streamed tarball; the cancel ladder
/// applies as it does to every process.
async fn extract(tar_rel: &str, ctx: &mut StepCtx) -> Result<Ending, StepFailure> {
    let workspace = ctx.env.workspace_path().to_string();
    let mut handle = ctx
        .env
        .spawn(executor::ProcessSpec {
            program: SmolStr::new("tar"),
            args: vec![
                SmolStr::new("-xf"),
                SmolStr::new(format!("{workspace}/{tar_rel}")),
                SmolStr::new("-C"),
                SmolStr::new(workspace),
            ],
            env: Default::default(),
            cwd: None,
        })
        .await
        .map_err(|e| checkout_error(format!("could not run `tar`: {e}")))?;
    if let Some(mut lines) = handle.lines() {
        let logs = ctx.logs.clone();
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                let _ = logs
                    .send(ir::StepEvent::Log {
                        stream: line.stream,
                        line: line.line,
                    })
                    .await;
            }
        });
    }
    let grace = ctx.env.grace();
    Ok(ladder(&mut *handle, &mut ctx.control, grace).await)
}

async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, StepFailure> {
    let mut command = tokio::process::Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| checkout_error(format!("could not run `git`: {e}")))?;
    if !output.status.success() {
        return Err(checkout_error(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn checkout_error(message: String) -> StepFailure {
    StepFailure {
        class: CHECKOUT_CLASS,
        message,
    }
}
