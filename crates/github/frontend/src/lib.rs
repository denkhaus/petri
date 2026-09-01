//! GitHub Actions workflow YAML → HIR.
//!
//! Workflow text in, `Graph` and diagnostics out. [`FileSource`] supplies local
//! reusable workflows. Manifest-backed actions stay as resolver nodes until
//! their steps run. Remote action references pin through [`ActionSource`] so
//! the graph still records the exact commit. [`GitHubActions`] is this format
//! as a [`Frontend`].
//!
//! # What a job becomes
//!
//! ```text
//!   J/start ──► J/step-1 ──► J/wait ──► … ──► J/last ──► J/done ──► (each dependent's start)
//!                    ▲                                     collector
//!                    └──── J/background ────┘
//! ```
//!
//! `J/start` is a noop whose precondition is the job's gate: `success()` over
//! its `needs` (unless its `if:` names a status function), and its `if:`.
//! `J/done` is a noop that summarizes the job — `{ result, outputs }` — from
//! the payload the last step's edge carries, and fans out to every job that
//! needs this one. A matrix job expands the region `J/start ..= J/last` per
//! combination; `J/done` sits outside and folds the legs. Every `needs.J.*` and
//! `success()` downstream reads `J/done`'s record, never a template edge.
//! A background step fans out from the foreground chain. A wait joins its
//! private completion, publishes the target's result and deferred environment
//! changes, and then advances the chain. An implicit wait joins any remaining
//! targets before action post phases.
//!
//! Step nodes carry no engine precondition: every step-level condition — an
//! `if:`, a `pre-if`, a `post-if` — lowers into the step's config as a **gate**
//! ([`gate`]) the step kind evaluates at spawn. The gate carries GitHub's
//! job-status semantics explicitly: `success()` in a step means "no earlier
//! step of this job failed and the job was not cancelled out from under it",
//! built as an expression over the earlier steps' run-context records, and
//! every step is additionally gated on the job having started. That is how a
//! false job `if:` skips every step, and how `continue-on-error` (a
//! `PartialSuccess`) does not fail the job. The gate is also what lets a
//! condition read `hashFiles(...)` and `env.*` — including values earlier steps
//! appended to `GITHUB_ENV` — which only the step, in the job environment, can
//! resolve.
//!
//! # Cancellation
//!
//! GitHub runs `if: always()` and `if: cancelled()` work after a cancellation.
//! Every node this frontend emits (except a matrix expansion head, which a
//! cancelled scope never splices) sets the structural `Node.run_on_cancel` flag
//! (spec §5), so after a polite cancel every remaining step fires and its gate
//! decides against the real state: `cancelled()` is true, `success()` is false.
//! Cleanup steps run; everything else self-skips, recording `Cancelled`. No
//! condition text is sniffed to decide admission. A job whose own gate admitted
//! it *after* the cancel — `if: always()` cleanup — records that on its
//! `start`, and its interior steps then evaluate normally, as GitHub's do.
//! `cancelled()` ORs in the `scope_cancelled` static, so a cancel that lands
//! between steps — or a `fail_fast` scope cancel, which root-only
//! `run.cancelled` cannot see — still reads as cancelled.
//!
//! # `runs-on: ${{ matrix.os }}`
//!
//! Resolved at lowering, once per leg ([`runs_on`]): the static matrix expands
//! through the same combinators the engine runs, each leg's labels go through
//! the same placement policy as literal labels, and the union becomes the
//! scope's requirements — sound because every supported label names a Linux
//! environment the one local executor provides, so the legs share a scope. The
//! per-leg results are preserved on the job's `start` node meta. What has no
//! value before the run — `github`, `needs`, `inputs`, a matrix whose legs are
//! themselves expressions — stays rejected as `unsupported.runs_on.expression`.
//! When labels can map to *different* environments (a configurable resolver),
//! legs whose labels differ will need their own scopes; the IR has one scope
//! per job, which remains a spec finding for that day.

pub mod action;
mod call;
mod composite;
pub mod expr_lower;
pub mod exprs;
pub mod gate;
pub mod identity;
mod inputs;
mod lower;
mod model;
mod runners;
mod runs_on;

use std::env::consts;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::{fs, mem};

pub use action::{
    ACTION_KIND, ActionSource, BACKGROUND_COMPLETE_KIND, BACKGROUND_PUBLISH_KIND,
    BACKGROUND_START_KIND, BACKGROUND_WAIT_KIND, CHECKOUT_KIND, DEFERRED_ACTION_KIND,
    DEFERRED_ACTION_POST_KIND, DEFERRED_ACTION_PUBLISH_KIND, DEFERRED_ACTION_RESULT_KIND,
    DOCKER_ACTION_KIND, REPO_PARAM_CONTEXT, REPO_PARAM_KEY, RUN_KIND, STATE_OUTPUT_KEY,
};
use frontend::yaml::Document;
use frontend::{Diagnostics, FileSource, Frontend, Lowered};
pub use lower::{DeferredActionPlan, PlannedDeferredAction, plan_deferred_action};
pub use runners::RunnerMap;
use serde_json::Value;
use smol_str::SmolStr;

/// Parse and lower a workflow file, with no source for `uses: owner/repo@ref`
/// actions: they are rejected as `unsupported.action.remote`.
pub fn load(file: &str, text: &str, files: &dyn FileSource) -> Lowered {
    load_with(file, text, files, None)
}

/// Parse and lower a workflow file. `actions` pins `uses: owner/repo@ref`
/// references while lowering. The action manifest and tree stay unread until
/// the step runs. The runner map is the built-in one; [`load_configured`]
/// takes the host's.
pub fn load_with(
    file: &str,
    text: &str,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
) -> Lowered {
    load_configured(file, text, files, actions, &RunnerMap::builtin(), true)
}

/// [`load_with`], with the host's [`RunnerMap`] deciding which `runs-on` labels
/// place on this machine, and the local-checkout switch:
/// `substitute_checkout` turns supportable `actions/checkout` calls into the
/// `github/checkout` step ([`CHECKOUT_KIND`]) — on in the shipped
/// configuration, off for a host that wants the real action every time.
pub fn load_configured(
    file: &str,
    text: &str,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    runners: &RunnerMap,
    substitute_checkout: bool,
) -> Lowered {
    let mut diags = Diagnostics::new();
    let Some(doc) = Document::parse(file, text, &mut diags) else {
        return Lowered::rejected(diags);
    };
    let Some(workflow) = model::read(&doc, &mut diags) else {
        return Lowered::rejected(diags);
    };
    lower::lower(
        &workflow,
        files,
        actions,
        runners,
        substitute_checkout,
        diags,
    )
}

/// GitHub Actions, as a [`Frontend`]: it claims anything under
/// `.github/workflows/`.
///
/// Without an [`ActionSource`] it lowers `run:` steps and local actions and
/// rejects actions from other repositories; with one, remote references pin
/// at load time. All action manifests resolve when their steps run. The
/// [`RunnerMap`] starts as the built-in `ubuntu-*` labels;
/// [`Self::with_runners`] installs the host's configuration.
pub struct GitHubActions {
    actions:             Option<Arc<dyn ActionSource>>,
    runners:             RunnerMap,
    substitute_checkout: bool,
}

impl Default for GitHubActions {
    fn default() -> Self {
        Self {
            actions:             None,
            runners:             RunnerMap::default(),
            substitute_checkout: true,
        }
    }
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
            ..Self::default()
        }
    }

    /// The host's runner-label map: which `runs-on` labels place here.
    #[must_use]
    pub fn with_runners(mut self, runners: RunnerMap) -> Self {
        self.runners = runners;
        self
    }

    /// The local-checkout off-switch: `false` keeps every `actions/checkout`
    /// the real action, credentials, network and all.
    #[must_use]
    pub fn with_checkout_substitution(mut self, substitute: bool) -> Self {
        self.substitute_checkout = substitute;
        self
    }
}

impl Frontend for GitHubActions {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `Frontend` trait fixes this signature; an impl cannot widen the \
                  returned lifetime"
    )]
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
        load_configured(
            file,
            text,
            files,
            self.actions.as_deref(),
            &self.runners,
            self.substitute_checkout,
        )
    }

    /// The `github`, `runner` and `vars` contexts a runner would supply.
    ///
    /// The repository slug comes from the checkout's `origin` remote — the same
    /// identity the lowering's placement guards evaluate against, so the two
    /// can never disagree. `github.sha`, `github.ref` and `github.ref_name`
    /// come from the checkout's actual HEAD: run parameters are host-filled
    /// at run start and recorded in the graph, so honest values here change
    /// no lowering and break no replay — and `actions/checkout` fetches a
    /// commit that exists instead of the fixed zero sha. Everything else
    /// stays fixed (a local run is not a real GitHub event), and without a
    /// checkout the fixed values stand in wholesale.
    ///
    /// The one delta this creates from lowering: a *condition* reading
    /// `github.sha` resolves to the zero sha in the placement statics and to
    /// the real commit at run time. Placement never reads HEAD by design — a
    /// commit must not change what a file lowers to.
    fn default_params(&self, repo: &Path) -> Vec<(SmolStr, Value)> {
        let slug = fs::read_to_string(repo.join(".git").join("config"))
            .ok()
            .and_then(|config| identity::slug_from_config(&config))
            .unwrap_or_else(|| {
                let name = repo
                    .file_name()
                    .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
                format!("local/{name}")
            });
        let mut github = identity::github_context(Some(&slug));
        if let Some(head) = identity::head_identity(repo) {
            github["sha"] = serde_json::json!(head.sha);
            if let Some(reference) = head.reference {
                if let Some(name) = reference.strip_prefix("refs/heads/") {
                    github["ref_name"] = serde_json::json!(name);
                }
                github["ref"] = serde_json::json!(reference);
            }
        }
        vec![
            (SmolStr::new("github"), github),
            (
                SmolStr::new("runner"),
                serde_json::json!({
                    "os": runner_os(consts::OS),
                    "arch": runner_arch(consts::ARCH),
                    "name": "local",
                }),
            ),
            (SmolStr::new("vars"), serde_json::json!({})),
            // Host facts for petri's own step kinds, not a GitHub context:
            // where the run's repository lives, for the `github/checkout`
            // substitute to materialize the workspace from.
            (
                SmolStr::new(REPO_PARAM_CONTEXT),
                serde_json::json!({ REPO_PARAM_KEY: repo.display().to_string() }),
            ),
        ]
    }

    /// A workflow file lives at `<repo>/.github/workflows/`, so the root is the
    /// nearest ancestor holding a `.github` directory — else the file's own
    /// directory, for a file that is not in a checkout at all.
    fn repo_root(&self, file: &Path) -> PathBuf {
        repo_root(file)
    }
}

/// `runner.os` as GitHub spells it: `Linux`, `macOS`, `Windows`. Workflows
/// compare against these literally, and actions read `RUNNER_OS`.
#[expect(
    clippy::match_same_arms,
    reason = "the arms are a lookup table from Rust's os names to GitHub's spelling; the \
              `linux` row states that mapping, and the wildcard is the separate default for \
              a platform this runner does not know"
)]
pub fn runner_os(os: &str) -> &'static str {
    match os {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        _ => "Linux",
    }
}

/// `runner.arch` as GitHub spells it: `X64`, `ARM64`, `X86`, `ARM`.
#[expect(
    clippy::match_same_arms,
    reason = "the arms are a lookup table from Rust's arch names to GitHub's spelling; the \
              `x86_64` row states that mapping, and the wildcard is the separate default for \
              an architecture this runner does not know"
)]
pub fn runner_arch(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "X64",
        "aarch64" => "ARM64",
        "x86" => "X86",
        "arm" => "ARM",
        _ => "X64",
    }
}

/// Split shell-ish text into words: whitespace separates, single and double
/// quotes group, a backslash escapes outside single quotes. How GitHub reads
/// `container.options`, `services.<id>.options` and a Docker step's
/// `with.args` — one splitter, shared by the lowering and the step kinds.
pub fn split_shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    words.push(mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    current.push(q);
                }
            }
            '"' => {
                started = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        }
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            other => {
                started = true;
                current.push(other);
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

fn repo_root(file: &Path) -> PathBuf {
    let mut dir = file
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
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

#[cfg(test)]
mod split_tests {
    use super::split_shell_words;

    #[test]
    fn words_split_like_a_shell() {
        assert_eq!(split_shell_words("a b  c"), vec!["a", "b", "c"]);
        assert_eq!(split_shell_words("'a b' c"), vec!["a b", "c"]);
        assert_eq!(split_shell_words(r#""a \"b\"" c"#), vec![r#"a "b""#, "c"]);
        assert_eq!(split_shell_words("one\\ arg"), vec!["one arg"]);
        assert_eq!(split_shell_words("  "), Vec::<String>::new());
        assert_eq!(split_shell_words("''"), vec![""]);
        assert_eq!(
            split_shell_words("--health-cmd pg_isready --health-interval 10s"),
            vec!["--health-cmd", "pg_isready", "--health-interval", "10s"]
        );
    }
}
