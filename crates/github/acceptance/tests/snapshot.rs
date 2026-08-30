//! Refresh the corpus action snapshot (network; run with `--ignored`).
//!
//! Lowers every corpus workflow with a recording source over git, so every
//! `uses: owner/repo@ref` the corpus makes — references nested in remote
//! composites included — is resolved to a commit and its manifest, and the lot
//! is written to `crates/github/corpus/actions-snapshot.json`. The offline
//! corpus run (`--test harness`) then resolves through the snapshot; see
//! `acceptance::SnapshotSource`.
//!
//! Like the corpus itself, the snapshot is fetched, not vendored: gitignored,
//! and reproducible from the corpus plus the state of the referenced
//! repositories.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use acceptance::{SnapshotEntry, SnapshotSource, check_one, has_corpus, workflows};
use frontend_gha::action::{ActionRef, ActionSource, ActionSourceError, PinnedAction};
use github_actions::GitActionSource;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

/// Passes every call to git and records what came back, keyed by the reference
/// as the lowering names it. A resolve that succeeds is recorded when its
/// manifest arrives; a failure at either step is recorded as failed.
struct Recording {
    inner: GitActionSource,
    seen:  Mutex<BTreeMap<String, SnapshotEntry>>,
}

impl Recording {
    fn record(&self, key: String, entry: SnapshotEntry) {
        self.seen.lock().expect("not poisoned").insert(key, entry);
    }
}

impl ActionSource for Recording {
    fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
        let result = self.inner.resolve(reference);
        if let Err(e) = &result {
            self.record(reference.to_string(), SnapshotEntry::Failed {
                error: e.to_string(),
            });
        }
        result
    }

    fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let result = self.inner.manifest(pinned);
        self.record(pinned.reference.to_string(), entry_for(pinned, &result));
        result
    }

    /// Called workflows fetch whole files; recorded exactly like manifests.
    fn file(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let result = self.inner.file(pinned);
        self.record(pinned.reference.to_string(), entry_for(pinned, &result));
        result
    }
}

fn entry_for(pinned: &PinnedAction, result: &Result<String, ActionSourceError>) -> SnapshotEntry {
    match result {
        Ok(text) => SnapshotEntry::Resolved {
            sha:      pinned.sha.to_string(),
            manifest: text.clone(),
        },
        Err(e) => SnapshotEntry::Failed {
            error: e.to_string(),
        },
    }
}

#[test]
#[ignore = "network: refreshes crates/github/corpus/actions-snapshot.json"]
#[expect(
    clippy::print_stderr,
    reason = "the refresh is run by hand and reports progress and failures on stderr"
)]
fn refresh_action_snapshot() {
    let root = corpus_root();
    assert!(
        has_corpus(&root),
        "fetch the corpus first: scripts/corpus-fetch.sh"
    );
    let recording = Arc::new(Recording {
        inner: GitActionSource::new(root.join(".actions-cache")),
        seen:  Mutex::new(BTreeMap::new()),
    });
    let source: Arc<dyn ActionSource> = Arc::clone(&recording) as Arc<dyn ActionSource>;
    for (repo, repo_root, file) in workflows(&root) {
        eprintln!("== {repo} {}", file.display());
        let _ = check_one(&repo, &repo_root, &file, Some(&source));
    }

    let seen = recording.seen.lock().expect("not poisoned");
    let path = SnapshotSource::write(&root, &seen).expect("write the snapshot");
    let failed = seen
        .values()
        .filter(|e| matches!(e, SnapshotEntry::Failed { .. }))
        .count();
    eprintln!(
        "wrote {} references ({failed} failed) to {}",
        seen.len(),
        path.display()
    );
    for (uses, entry) in seen.iter() {
        if let SnapshotEntry::Failed { error } = entry {
            eprintln!("   failed: {uses}: {error}");
        }
    }
}
