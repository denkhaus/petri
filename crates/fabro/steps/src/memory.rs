//! Which project instruction files an LLM node reads, as Fabro selects
//! them, and where they are found in the execution scope.
//!
//! Fabro's `discover_memory` picks filenames by agent profile and walks the
//! directories from the Git root down to the working directory, root first.
//! Petri selects the same paths and hands the ordered list to Pebble's
//! loader, which owns the 32,768-byte budget, exact-content deduplication,
//! empty-file handling and truncation: a native agent session loads through
//! `CodingAgentOptions::with_memory_files`, a prompt node through the same
//! `ProjectMemory::load` over the scope. A prompt node honors
//! `project_memory=false` and, when enabled, reads only the working
//! directory. Both paths use [`select`]; the existence check runs one shell
//! command in the scope so no missing file is ever asked for.

use std::iter;
use std::path::Path;
use std::sync::Arc;

use executor::{ExecEnv, OutputMode, ProcessSpec};
use tokio::time::{Duration, timeout};

/// How long the scope probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Which directories a node reads instruction files from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// From the Git root down to the working directory, root first: what a
    /// native agent session reads.
    GitRootToWorkingDir,
    /// The working directory alone: what a prompt node reads.
    WorkingDirOnly,
}

/// Fabro's instruction filenames for an agent profile, in load order.
/// Profiles are Pebble's names (`anthropic`, `claude-5`, `openai`, `gpt56`,
/// `gpt6`, `gemini`, `kimi`); anything else reads `AGENTS.md` alone.
pub fn filenames(profile: &str) -> &'static [&'static str] {
    match profile {
        "anthropic" | "claude-5" => &["AGENTS.md", "CLAUDE.md"],
        "openai" | "gpt56" | "gpt6" => &["AGENTS.md", ".codex/instructions.md"],
        "gemini" => &["AGENTS.md", "GEMINI.md"],
        _ => &["AGENTS.md"],
    }
}

/// The ordered candidate paths for `profile`, relative to the working
/// directory: one directory level per component from the Git root down to
/// the working directory, each with the profile's filenames in order.
/// `git_root` is the Git root's path as the scope sees it, when the working
/// directory is inside one; the working directory is `working_dir`.
pub fn candidates(
    profile: &str,
    git_root: Option<&str>,
    working_dir: &str,
    scope: Scope,
) -> Vec<String> {
    let names = filenames(profile);
    let mut dirs: Vec<String> = Vec::new();
    match scope {
        Scope::WorkingDirOnly => dirs.push(String::new()),
        Scope::GitRootToWorkingDir => {
            let relative = git_root.and_then(|root| {
                Path::new(working_dir)
                    .strip_prefix(root)
                    .ok()
                    .map(|rest| rest.components().count())
            });
            match relative {
                Some(depth) => {
                    // Root first: `../..`, then `..`, then the working dir.
                    for level in (0..=depth).rev() {
                        dirs.push(iter::repeat_n("..", level).collect::<Vec<_>>().join("/"));
                    }
                }
                None => dirs.push(String::new()),
            }
        }
    }
    let mut out = Vec::new();
    for dir in dirs {
        for name in names {
            out.push(if dir.is_empty() {
                (*name).to_owned()
            } else {
                format!("{dir}/{name}")
            });
        }
    }
    out
}

/// The Git root above the scope's working directory, as the scope sees it.
/// `None` when the working directory is not in a repository or Git is not
/// available there.
pub async fn git_root(env: &dyn ExecEnv) -> Option<String> {
    let output = run(env, "git rev-parse --show-toplevel 2>/dev/null").await?;
    let root = output.trim();
    (!root.is_empty()).then(|| root.to_owned())
}

/// The candidate paths that exist in the scope, in order. One shell command
/// probes them all, so a missing file is never read.
pub async fn existing(env: &dyn ExecEnv, candidates: &[String]) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let script = candidates
        .iter()
        .map(|path| {
            format!(
                "if [ -f {p} ]; then printf '%s\\n' {p}; fi",
                p = quote(path)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Some(output) = run(env, &script).await else {
        return Vec::new();
    };
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// [`candidates`] for the scope, filtered to the files that exist.
pub async fn select(env: &dyn ExecEnv, profile: &str, scope: Scope) -> Vec<String> {
    let root = match scope {
        Scope::GitRootToWorkingDir => git_root(env).await,
        Scope::WorkingDirOnly => None,
    };
    let candidates = candidates(profile, root.as_deref(), env.workspace_path(), scope);
    existing(env, &candidates).await
}

/// Run `script` in the scope and return its stdout when it succeeds.
pub(crate) async fn run(env: &dyn ExecEnv, script: &str) -> Option<String> {
    let spec = ProcessSpec::new("bash", &["-c", script]).with_output(OutputMode::Bytes);
    let mut handle = env.spawn(spec).await.ok()?;
    let mut bytes = handle.bytes()?;
    let drain = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(chunk) = bytes.recv().await {
            if chunk.stream == ir::LogStream::Stdout {
                out.extend(chunk.bytes);
            }
        }
        out
    });
    let status = timeout(PROBE_TIMEOUT, handle.wait()).await.ok()?.ok()?;
    let out = drain.await.ok()?;
    status
        .is_success()
        .then(|| String::from_utf8_lossy(&out).into_owned())
}

pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A shared environment handle, for callers holding an `Arc`.
pub async fn select_in(env: &Arc<dyn ExecEnv>, profile: &str, scope: Scope) -> Vec<String> {
    select(env.as_ref(), profile, scope).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_pick_fabro_filenames() {
        assert_eq!(filenames("anthropic"), ["AGENTS.md", "CLAUDE.md"]);
        assert_eq!(filenames("claude-5"), ["AGENTS.md", "CLAUDE.md"]);
        assert_eq!(filenames("openai"), ["AGENTS.md", ".codex/instructions.md"]);
        assert_eq!(filenames("gpt56"), ["AGENTS.md", ".codex/instructions.md"]);
        assert_eq!(filenames("gemini"), ["AGENTS.md", "GEMINI.md"]);
        assert_eq!(filenames("kimi"), ["AGENTS.md"]);
        assert_eq!(filenames("other"), ["AGENTS.md"]);
    }

    #[test]
    fn candidates_walk_root_first_then_the_working_dir() {
        let paths = candidates(
            "anthropic",
            Some("/repo"),
            "/repo/crates/app",
            Scope::GitRootToWorkingDir,
        );
        assert_eq!(paths, [
            "../../AGENTS.md",
            "../../CLAUDE.md",
            "../AGENTS.md",
            "../CLAUDE.md",
            "AGENTS.md",
            "CLAUDE.md",
        ]);
        assert_eq!(
            candidates("gemini", Some("/repo"), "/repo", Scope::GitRootToWorkingDir),
            ["AGENTS.md", "GEMINI.md"]
        );
        assert_eq!(
            candidates("gemini", None, "/w", Scope::GitRootToWorkingDir),
            ["AGENTS.md", "GEMINI.md"]
        );
        assert_eq!(
            candidates("openai", Some("/repo"), "/repo/a", Scope::WorkingDirOnly),
            ["AGENTS.md", ".codex/instructions.md"]
        );
    }
}
