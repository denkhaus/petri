//! The compatibility corpus harness.
//!
//! Runs every corpus workflow through the GHA frontend and classifies the result.
//! The bar is not 100% lowering. It is: every workflow either lowers, or is rejected
//! with a specific `unsupported.*` code — zero panics, zero generic errors. The report
//! also counts which actions the corpus uses most, so the action runner's gaps are
//! ranked by how much they block.
//!
//! The corpus data is not committed. `scripts/corpus-fetch.sh` downloads it, so on a
//! fresh clone it is simply absent, and the corpus tests skip themselves rather than
//! fail — see [`corpus_present`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use frontend::{Diagnostic, DirFiles, Frontend, Severity};
use frontend_gha::GitHubActions;
use frontend_gha::action::{ActionRef, ActionSource, ActionSourceError, PinnedAction};
use smol_str::SmolStr;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Lowered with no diagnostics at all.
    Clean,
    /// Lowered; warnings only.
    Warnings,
    /// Rejected, and every error is an `unsupported.*` code.
    Unsupported,
    /// Rejected with at least one error that is not `unsupported.*` — a reader gap,
    /// or a genuinely malformed file. Investigate.
    OtherError,
    /// The frontend panicked. Never acceptable.
    Panicked,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub repo: String,
    pub file: String,
    pub class: Class,
    pub diagnostics: Vec<Diagnostic>,
    pub nodes: usize,
    pub panic: Option<String>,
}

/// Features that put a workflow out of the corpus's scope entirely: Windows and
/// macOS runners and their shells. The local executor emulates Linux, these
/// workflows can never lower here by policy, and counting them as "rejected"
/// reads as pressure where there is none — the report keeps them out of the
/// compatibility denominator.
pub const OUT_OF_SCOPE: &[&str] = &[
    "runs_on.windows",
    "runs_on.macos",
    "shell.cmd",
    "shell.powershell",
];

/// A reusable file whose sole blocker, standalone, is a runner input only a
/// caller provides. Not a gap: every call site resolves it (its in-repo
/// callers lower), and no standalone lowering could ever place it. Leaves the
/// denominator the way Windows/macOS workflows do.
pub const CALLEE_ONLY: &[&str] = &["runs_on.callee_input"];

/// The referenced repository is gone upstream (private or removed, recorded at
/// snapshot refresh): the workflow is broken on GitHub itself, so it measures
/// nothing about this runner. Leaves the denominator.
pub const BROKEN_UPSTREAM: &[&str] = &["action.upstream_gone"];

impl Outcome {
    /// Every `unsupported.*` feature this workflow hit, deduplicated.
    pub fn unsupported_features(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .diagnostics
            .iter()
            .filter_map(|d| d.unsupported_feature().map(str::to_string))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The workflow needs a Windows or macOS runner or shell somewhere, so it is
    /// out of this corpus's scope by policy, whatever else it would need.
    pub fn out_of_scope(&self) -> bool {
        self.unsupported_features()
            .iter()
            .any(|f| OUT_OF_SCOPE.contains(&f.as_str()))
    }

    /// Every rejection is [`CALLEE_ONLY`]: a reusable file only its callers can
    /// place. "Every", not "any" — a file with another blocker besides is a
    /// real rejection and stays one.
    pub fn callee_only(&self) -> bool {
        let features = self.unsupported_features();
        self.class == Class::Unsupported
            && !features.is_empty()
            && features.iter().all(|f| CALLEE_ONLY.contains(&f.as_str()))
    }

    /// The workflow references a repository that is gone upstream
    /// ([`BROKEN_UPSTREAM`]): broken on GitHub itself, whatever else it needs.
    pub fn broken_upstream(&self) -> bool {
        self.unsupported_features()
            .iter()
            .any(|f| BROKEN_UPSTREAM.contains(&f.as_str()))
    }

    /// Remote actions this workflow references, as `owner/repo@ref`.
    pub fn remote_actions(&self) -> Vec<String> {
        self.diagnostics
            .iter()
            .filter(|d| d.code == "unsupported.action.remote")
            .map(|d| d.message.clone())
            .collect()
    }

    pub fn other_errors(&self) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error && d.unsupported_feature().is_none())
            .collect()
    }
}

/// Is the corpus fetched?
///
/// True when `root` exists and at least one repo directory under it has a
/// `.github/workflows` directory. The corpus is gitignored, so a fresh clone has
/// none of it until `scripts/corpus-fetch.sh` runs.
///
/// The convention, mirroring `PETRI_REQUIRE_DOCKER` for the Docker battery: a corpus
/// test skips with a message when this is false, and `PETRI_REQUIRE_CORPUS` turns that
/// skip into a failure. CI sets it, because a silently skipped battery is
/// indistinguishable from a passing one.
pub fn corpus_present(root: &Path) -> bool {
    let Ok(repos) = std::fs::read_dir(root) else {
        return false;
    };
    repos
        .flatten()
        .any(|e| e.path().join(".github").join("workflows").is_dir())
}

/// Every workflow under `crates/github/corpus/*/.github/workflows/`.
pub fn workflows(corpus_root: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(repos) = std::fs::read_dir(corpus_root) else {
        return out;
    };
    let mut repos: Vec<_> = repos.flatten().filter(|e| e.path().is_dir()).collect();
    repos.sort_by_key(|e| e.file_name());
    for repo in repos {
        let repo_root = repo.path();
        let dir = repo_root.join(".github").join("workflows");
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = files
            .flatten()
            .filter(|f| {
                let name = f.file_name().to_string_lossy().into_owned();
                name.ends_with(".yml") || name.ends_with(".yaml")
            })
            .collect();
        files.sort_by_key(|f| f.file_name());
        for file in files {
            out.push((
                repo.file_name().to_string_lossy().replace("__", "/"),
                repo_root.clone(),
                file.path(),
            ));
        }
    }
    out
}

/// One reference in the action snapshot: resolved to a commit and its manifest, or
/// the error the refresh hit.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(untagged)]
pub enum SnapshotEntry {
    Resolved { sha: String, manifest: String },
    Failed { error: String },
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RawSnapshotEntry {
    Resolved(ResolvedSnapshotEntry),
    Failed(FailedSnapshotEntry),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedSnapshotEntry {
    sha: String,
    manifest: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedSnapshotEntry {
    error: String,
}

impl<'de> serde::Deserialize<'de> for SnapshotEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match RawSnapshotEntry::deserialize(deserializer)? {
            RawSnapshotEntry::Resolved(entry) => SnapshotEntry::Resolved {
                sha: entry.sha,
                manifest: entry.manifest,
            },
            RawSnapshotEntry::Failed(entry) => SnapshotEntry::Failed { error: entry.error },
        })
    }
}

/// The offline action source for the corpus: every `uses:` reference the corpus
/// makes, resolved once over the network by the refresh test
/// (`--test snapshot -- --ignored`) and read back here with none. A reference the
/// snapshot lacks answers `Unavailable` with no reason (a refresh may add it);
/// one whose refresh failed answers `Unavailable` carrying the recorded upstream
/// error (a private or removed repository — a refresh will not help). Either way
/// the workflow classifies exactly as it would with no source at all:
/// `unsupported.action.remote`, with a hint that now says which case it is.
pub struct SnapshotSource {
    entries: BTreeMap<String, SnapshotEntry>,
}

impl SnapshotSource {
    /// The snapshot's file name under the corpus root. Gitignored with the rest of
    /// the corpus data: fetched, not vendored.
    pub const FILE: &'static str = "actions-snapshot.json";

    /// Load `<corpus>/actions-snapshot.json`. `None` when it has not been written;
    /// a malformed file is a loud failure, not a quiet no-source run.
    pub fn load(corpus_root: &Path) -> Option<Self> {
        let path = corpus_root.join(Self::FILE);
        let text = std::fs::read_to_string(&path).ok()?;
        let entries = serde_json::from_str(&text)
            .expect("the action snapshot is a map of resolved or failed references");
        Some(Self { entries })
    }

    /// How many references the snapshot holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many of them failed to resolve when the snapshot was written.
    pub fn failed(&self) -> usize {
        self.entries
            .values()
            .filter(|e| matches!(e, SnapshotEntry::Failed { .. }))
            .count()
    }

    /// Write `<corpus>/actions-snapshot.json` — the refresh test's half of the
    /// format. Returns where it went.
    pub fn write(
        corpus_root: &Path,
        entries: &BTreeMap<String, SnapshotEntry>,
    ) -> std::io::Result<PathBuf> {
        let path = corpus_root.join(Self::FILE);
        let text = serde_json::to_string_pretty(entries).expect("the snapshot encodes");
        std::fs::write(&path, text)?;
        Ok(path)
    }
}

impl SnapshotSource {
    /// The `Unavailable` answer for the entry a lookup found: with the refresh's
    /// recorded error as the reason when the reference failed, with none when it
    /// is simply absent.
    fn unavailable(reference: String, entry: Option<&SnapshotEntry>) -> ActionSourceError {
        let reason = match entry {
            Some(SnapshotEntry::Failed { error }) => Some(error.clone()),
            _ => None,
        };
        ActionSourceError::Unavailable { reference, reason }
    }
}

impl ActionSource for SnapshotSource {
    fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
        let key = reference.to_string();
        match self.entries.get(&key) {
            Some(SnapshotEntry::Resolved { sha, .. }) => Ok(PinnedAction {
                reference: reference.clone(),
                sha: SmolStr::new(sha),
            }),
            other => Err(Self::unavailable(key, other)),
        }
    }

    fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let key = pinned.reference.to_string();
        match self.entries.get(&key) {
            Some(SnapshotEntry::Resolved { manifest, .. }) => Ok(manifest.clone()),
            other => Err(Self::unavailable(key, other)),
        }
    }

    /// A remote called workflow's text: the refresh records it in the same
    /// `manifest` slot, keyed by the full reference.
    fn file(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        self.manifest(pinned)
    }
}

/// Lower one workflow, catching panics so a crash is a classified result rather than
/// a dead harness. `actions` resolves `uses: owner/repo@ref`; without it they are
/// rejected as `action.remote`.
pub fn check_one(
    repo: &str,
    repo_root: &Path,
    file: &Path,
    actions: Option<&Arc<dyn ActionSource>>,
) -> Outcome {
    let rel = file
        .strip_prefix(repo_root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            return Outcome {
                repo: repo.to_string(),
                file: rel,
                class: Class::OtherError,
                diagnostics: vec![Diagnostic::error(
                    "io",
                    frontend::Span::file(file.to_string_lossy().as_ref()),
                    e.to_string(),
                )],
                nodes: 0,
                panic: None,
            };
        }
    };
    let files = DirFiles {
        root: repo_root.to_path_buf(),
    };
    let format = match actions {
        Some(actions) => GitHubActions::with_actions(Arc::clone(actions)),
        None => GitHubActions::new(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        format.load(&rel, &text, &files)
    }));
    match result {
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            Outcome {
                repo: repo.to_string(),
                file: rel,
                class: Class::Panicked,
                diagnostics: Vec::new(),
                nodes: 0,
                panic: Some(message),
            }
        }
        Ok(lowered) => {
            let diagnostics = lowered.diagnostics.into_vec();
            let errors: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.is_error()).collect();
            let class = if let Some(_graph) = &lowered.graph {
                if diagnostics.is_empty() {
                    Class::Clean
                } else {
                    Class::Warnings
                }
            } else if errors.iter().all(|d| d.unsupported_feature().is_some()) {
                Class::Unsupported
            } else {
                Class::OtherError
            };
            Outcome {
                repo: repo.to_string(),
                file: rel,
                class,
                nodes: lowered.graph.as_ref().map_or(0, |g| g.nodes.len()),
                diagnostics,
                panic: None,
            }
        }
    }
}

pub fn check_all(corpus_root: &Path, actions: Option<&Arc<dyn ActionSource>>) -> Vec<Outcome> {
    workflows(corpus_root)
        .into_iter()
        .map(|(repo, root, file)| check_one(&repo, &root, &file, actions))
        .collect()
}

/// The report, as Markdown.
///
/// `census` is a run of the same corpus with no action source: its
/// `action.remote` rejections name every remote `uses:` reference, which is where
/// the actions-by-frequency table comes from. With no snapshot the two runs are the
/// same and callers pass `outcomes` twice. `actions_note` says how remote actions
/// were resolved for this report.
pub fn report(outcomes: &[Outcome], census: &[Outcome], actions_note: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    // Three exclusions leave the denominator entirely, so every share below
    // measures what this runner could ever run: Windows/macOS workflows (out
    // of scope by policy), reusable-only files whose runner a caller provides
    // (nothing standalone to place), and workflows whose referenced repository
    // is gone upstream (broken on GitHub itself). Precedence in that order, so
    // a file lands in exactly one class.
    let out_of_scope = outcomes.iter().filter(|o| o.out_of_scope()).count();
    let broken_upstream = outcomes
        .iter()
        .filter(|o| !o.out_of_scope() && o.broken_upstream())
        .count();
    let callee_only = outcomes
        .iter()
        .filter(|o| !o.out_of_scope() && !o.broken_upstream() && o.callee_only())
        .count();
    let in_scope: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| !o.out_of_scope() && !o.broken_upstream() && !o.callee_only())
        .collect();
    let total = in_scope.len();
    let count = |c: Class| in_scope.iter().filter(|o| o.class == c).count();
    let _ = writeln!(out, "# Compatibility corpus report\n");
    let _ = writeln!(
        out,
        "{} workflows from {} repositories. Out of the denominator by policy: \
         {out_of_scope} need a Windows or macOS runner or shell, {callee_only} are \
         reusable-only files whose runner a caller provides (callee-only), and \
         {broken_upstream} reference a repository gone upstream (broken on GitHub \
         itself) — leaving **{total} in scope**.\n",
        outcomes.len(),
        {
            let mut repos: Vec<&str> = outcomes.iter().map(|o| o.repo.as_str()).collect();
            repos.sort();
            repos.dedup();
            repos.len()
        }
    );
    let _ = writeln!(out, "{actions_note}\n");
    let _ = writeln!(out, "| Result (of the in-scope {total}) | Count | Share |");
    let _ = writeln!(out, "|---|---|---|");
    for (label, class) in [
        ("lowered clean", Class::Clean),
        ("lowered with warnings", Class::Warnings),
        (
            "rejected with a specific `unsupported.*` code",
            Class::Unsupported,
        ),
        ("**failed for any other reason**", Class::OtherError),
        ("**panicked**", Class::Panicked),
    ] {
        let n = count(class);
        let _ = writeln!(
            out,
            "| {label} | {n} | {:.0}% |",
            if total == 0 {
                0.0
            } else {
                100.0 * n as f64 / total as f64
            }
        );
    }

    // Which actions the runner meets most, counted from the sourceless census.
    let mut actions: BTreeMap<String, usize> = BTreeMap::new();
    let mut actions_versioned: BTreeMap<String, usize> = BTreeMap::new();
    for o in census {
        for a in o.remote_actions() {
            *actions_versioned.entry(a.clone()).or_default() += 1;
            let bare = a.split('@').next().unwrap_or(&a).to_string();
            *actions.entry(bare).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&String, &usize)> = actions.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let _ = writeln!(out, "\n## Remote actions, by frequency\n");
    let _ = writeln!(
        out,
        "Which actions the runner meets most. `uses:` references across all workflows; a workflow using an action three times counts three.\n"
    );
    let _ = writeln!(out, "| Action | Uses |");
    let _ = writeln!(out, "|---|---|");
    for (name, n) in ranked.iter().take(40) {
        let _ = writeln!(out, "| `{name}` | {n} |");
    }
    let _ = writeln!(
        out,
        "\n{} distinct actions ({} distinct pinned refs).",
        actions.len(),
        actions_versioned.len()
    );

    // Every unsupported feature, by frequency (workflows, not occurrences) —
    // counted over the in-scope workflows only, so a feature also blocking a
    // Windows workflow is not inflated by pressure that could never pay off.
    let mut features: BTreeMap<String, usize> = BTreeMap::new();
    for o in &in_scope {
        for f in o.unsupported_features() {
            *features.entry(f).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&String, &usize)> = features.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let _ = writeln!(
        out,
        "\n## Unsupported features, by in-scope workflows affected\n"
    );
    let _ = writeln!(out, "| Feature | Workflows |");
    let _ = writeln!(out, "|---|---|");
    for (name, n) in &ranked {
        let _ = writeln!(out, "| `{name}` | {n} |");
    }

    // Anything that failed for a reason other than a specific rejection.
    let others: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| matches!(o.class, Class::OtherError | Class::Panicked))
        .collect();
    let _ = writeln!(out, "\n## Failures that are not specific rejections\n");
    if others.is_empty() {
        let _ = writeln!(
            out,
            "None. Every workflow either lowered or was rejected with a specific code."
        );
    } else {
        for o in others {
            let _ = writeln!(out, "### {} — {}\n", o.repo, o.file);
            if let Some(p) = &o.panic {
                let _ = writeln!(out, "**PANIC:** `{p}`\n");
            }
            for d in o.other_errors() {
                let _ = writeln!(out, "- `{}` {}", d.code, d.message);
            }
            let _ = writeln!(out);
        }
    }

    // Per-workflow table.
    let _ = writeln!(out, "\n## Every workflow\n");
    let _ = writeln!(
        out,
        "| Repository | Workflow | Result | Nodes | Unsupported |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|");
    for o in outcomes {
        // Excluded rows keep their place in the inventory but carry no
        // feature list: what else they would need is noise by policy.
        let excluded = o.out_of_scope() || o.broken_upstream() || o.callee_only();
        let class = if o.out_of_scope() {
            "out of scope"
        } else if o.broken_upstream() {
            "broken upstream"
        } else if o.callee_only() {
            "callee only"
        } else {
            match o.class {
                Class::Clean => "clean",
                Class::Warnings => "warnings",
                Class::Unsupported => "unsupported",
                Class::OtherError => "**other error**",
                Class::Panicked => "**PANIC**",
            }
        };
        let features = if excluded {
            Vec::new()
        } else {
            o.unsupported_features()
        };
        let _ = writeln!(
            out,
            "| {} | `{}` | {class} | {} | {} |",
            o.repo,
            o.file.trim_start_matches(".github/workflows/"),
            if o.nodes > 0 {
                o.nodes.to_string()
            } else {
                "—".to_string()
            },
            if features.is_empty() {
                "—".to_string()
            } else {
                features
                    .iter()
                    .map(|f| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }
    out
}

/// Workflows whose every step is a plain `run:` — candidates for running end to end.
pub fn run_only_candidates(outcomes: &[Outcome]) -> Vec<&Outcome> {
    outcomes
        .iter()
        .filter(|o| matches!(o.class, Class::Clean | Class::Warnings))
        .collect()
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_entries_have_one_exact_shape() {
        let resolved: SnapshotEntry =
            serde_json::from_str(r#"{"sha":"abc","manifest":"runs: {}"}"#).unwrap();
        assert!(matches!(resolved, SnapshotEntry::Resolved { .. }));
        assert!(
            serde_json::from_str::<SnapshotEntry>(
                r#"{"sha":"abc","manifest":"runs: {}","error":"also failed"}"#
            )
            .is_err()
        );
    }
}
