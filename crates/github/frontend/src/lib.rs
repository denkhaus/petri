//! GitHub Actions workflow YAML → HIR.
//!
//! Pure: text in, `Graph` and diagnostics out. The only IO is reading the workflow's
//! local composite actions, and that goes through [`FileSource`] so the caller decides
//! what "the repository" is — a directory, or a map in a test. [`GitHubActions`] is
//! this format as a [`Frontend`].
//!
//! # What a job becomes
//!
//! ```text
//!   J/start ──► J/step-1 ──► J/step-2 ──► … ──► J/last ──► J/done ──► (each dependent's start)
//!   gate         steps, chained by plain `next:`           collector
//! ```
//!
//! `J/start` is a noop whose precondition is the job's gate: `success()` over its
//! `needs` (unless its `if:` names a status function), and its `if:`. `J/done` is a
//! noop that summarizes the job — `{ result, outputs }` — from the payload the last
//! step's edge carries, and fans out to every job that needs this one. A matrix job
//! expands the region `J/start ..= J/last` per combination; `J/done` sits outside and
//! folds the legs. Every `needs.J.*` and `success()` downstream reads `J/done`'s
//! record, never a template edge.
//!
//! Step preconditions carry GitHub's job-status semantics explicitly: `success()` in a
//! step means "no earlier step of this job failed", built as an expression over the
//! earlier steps' run-context records, and every step is additionally gated on the
//! job having started. That is how a false job `if:` skips every step, and how
//! `continue-on-error` (a `PartialSuccess`) does not fail the job.
//!
//! # Cancellation
//!
//! GitHub runs `if: always()` and `if: cancelled()` work after a cancellation, and
//! the engine admits post-cancel work only through the structural
//! `Node.run_on_cancel` flag (spec §5). The lowering sets it where GitHub would
//! keep going: on a step whose `if:` names `always()` or `cancelled()`; on every
//! node of a job whose own `if:` does; on every inlined node of a local composite
//! whose caller's `if:` does; and on every `done` node unconditionally, because a
//! dependent's `needs.J.*` reads need a truthful summary in every cancel case.
//! `cancelled()` also ORs in the `scope_cancelled` static, so a cancel that lands
//! between steps — or a `fail_fast` scope cancel, which root-only `run.cancelled`
//! cannot see — still reads as cancelled.
//!
//! # One thing the engine cannot do today
//!
//! Reported as a spec finding rather than worked around:
//!
//! - **`runs-on: ${{ matrix.os }}`.** Each matrix leg would need its own environment,
//!   and the IR has one scope per job. Rejected as `unsupported.runs_on.expression`.

pub mod action;
pub mod composite;
pub mod expr_lower;
pub mod exprs;
pub mod lower;
pub mod model;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use frontend::{Diagnostics, FileSource, Frontend, Lowered};
use serde_json::Value;
use smol_str::SmolStr;

pub use action::{ACTION_KIND, ActionSource, RUN_KIND, STATE_OUTPUT_KEY};

/// Parse and lower a workflow file, with no source for `uses: owner/repo@ref`
/// actions: they are rejected as `unsupported.action.remote`.
pub fn load(file: &str, text: &str, files: &dyn FileSource) -> Lowered {
    load_with(file, text, files, None)
}

/// Parse and lower a workflow file. `actions` resolves `uses: owner/repo@ref`
/// references while lowering, so the graph pins the commit each one runs.
pub fn load_with(
    file: &str,
    text: &str,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
) -> Lowered {
    let mut diags = Diagnostics::new();
    let Some(doc) = frontend::yaml::Document::parse(file, text, &mut diags) else {
        return Lowered::rejected(diags);
    };
    let Some(workflow) = model::read(&doc, &mut diags) else {
        return Lowered::rejected(diags);
    };
    lower::lower(&workflow, files, actions, diags)
}

/// GitHub Actions, as a [`Frontend`]: it claims anything under `.github/workflows/`.
///
/// Without an [`ActionSource`] it lowers `run:` steps and local composites and
/// rejects actions from other repositories; with one, those resolve at load time.
#[derive(Default)]
pub struct GitHubActions {
    actions: Option<Arc<dyn ActionSource>>,
}

impl GitHubActions {
    /// The format with no way to reach other repositories' actions.
    pub fn new() -> Self {
        Self::default()
    }

    /// The format with `uses: owner/repo@ref` resolved through `actions`.
    pub fn with_actions(actions: Arc<dyn ActionSource>) -> Self {
        Self {
            actions: Some(actions),
        }
    }
}

impl Frontend for GitHubActions {
    fn name(&self) -> &str {
        "gha"
    }

    fn claims(&self, path: &Path) -> bool {
        let parts: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();
        parts.windows(2).any(|w| w == [".github", "workflows"])
    }

    fn load(&self, file: &str, text: &str, files: &dyn FileSource) -> Lowered {
        load_with(file, text, files, self.actions.as_deref())
    }

    /// The `github`, `runner` and `vars` contexts a runner would supply. Fixed
    /// values — a local run is not a real GitHub event — so the same file lowers to
    /// the same graph every time and a saved log replays against it.
    fn default_params(&self, repo: &Path) -> Vec<(SmolStr, Value)> {
        let name = repo
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_string());
        vec![
            (
                SmolStr::new("github"),
                serde_json::json!({
                    "repository": format!("local/{name}"),
                    "event_name": "workflow_dispatch",
                    "actor": "petri",
                    "ref": "refs/heads/main",
                    "ref_name": "main",
                    "sha": "0000000000000000000000000000000000000000",
                    "run_id": "1",
                    "run_number": "1",
                    "server_url": "https://github.com",
                    "api_url": "https://api.github.com",
                    "graphql_url": "https://api.github.com/graphql",
                }),
            ),
            (
                SmolStr::new("runner"),
                serde_json::json!({
                    "os": runner_os(std::env::consts::OS),
                    "arch": runner_arch(std::env::consts::ARCH),
                    "name": "local",
                }),
            ),
            (SmolStr::new("vars"), serde_json::json!({})),
        ]
    }

    /// A workflow file lives at `<repo>/.github/workflows/`, so the root is the
    /// nearest ancestor holding a `.github` directory — else the file's own
    /// directory, for a file that is not in a checkout at all.
    fn repo_root(&self, file: &Path) -> PathBuf {
        repo_root(file)
    }
}

/// `runner.os` as GitHub spells it: `Linux`, `macOS`, `Windows`. Workflows compare
/// against these literally, and actions read `RUNNER_OS`.
pub fn runner_os(os: &str) -> &'static str {
    match os {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        _ => "Linux",
    }
}

/// `runner.arch` as GitHub spells it: `X64`, `ARM64`, `X86`, `ARM`.
pub fn runner_arch(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "X64",
        "aarch64" => "ARM64",
        "x86" => "X86",
        "arm" => "ARM",
        _ => "X64",
    }
}

fn repo_root(file: &Path) -> PathBuf {
    let mut dir = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let start = dir.clone();
    loop {
        if dir.join(".github").is_dir() {
            return dir;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return start,
        }
    }
}
