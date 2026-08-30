//! Handoff §7 test 8: the corpus bar.
//!
//! Every corpus workflow either lowers or is rejected with a specific
//! `unsupported.*` code. Zero panics, zero generic errors. The report is
//! written to `crates/github/corpus/REPORT.md` on every run so it stays current
//! with the code.
//!
//! The corpus itself is fetched, not committed, so this skips when it is
//! absent. See `acceptance::corpus_present`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, fs};

use acceptance::{Class, SnapshotSource, check_all, corpus_present, report};
use frontend_gha::ActionSource;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr; a test binary has no other sink"
)]
fn every_corpus_workflow_lowers_or_is_rejected_specifically() {
    let root = corpus_root();
    if !corpus_present(&root) {
        assert!(
            !env::var("PETRI_REQUIRE_CORPUS").is_ok_and(|v| !v.is_empty()),
            "PETRI_REQUIRE_CORPUS is set, but the corpus is not fetched"
        );
        eprintln!("skipping: corpus not fetched; run scripts/corpus-fetch.sh");
        return;
    }

    // The sourceless run is the census of remote `uses:` references; with a
    // snapshot present, the classification run resolves them through it.
    let census = check_all(&root, None);
    let (outcomes, note) = match SnapshotSource::load(&root) {
        Some(snapshot) => {
            let note = format!(
                "Remote `uses:` references resolve through the action snapshot: \
                 {} references, {} of them unavailable. Refresh with \
                 `cargo test -p petri-github-acceptance --test snapshot -- --ignored`.",
                snapshot.len(),
                snapshot.failed()
            );
            let source: Arc<dyn ActionSource> = Arc::new(snapshot);
            (check_all(&root, Some(&source)), note)
        }
        None => (
            census.clone(),
            "No action snapshot: remote `uses:` references are rejected as \
             `action.remote`. Write one with \
             `cargo test -p petri-github-acceptance --test snapshot -- --ignored`."
                .to_string(),
        ),
    };
    assert!(
        outcomes.len() > 100,
        "the corpus is fetched: {} workflows",
        outcomes.len()
    );

    let markdown = report(&outcomes, &census, &note);
    fs::write(root.join("REPORT.md"), &markdown).expect("write the report");

    let panics: Vec<_> = outcomes
        .iter()
        .filter(|o| o.class == Class::Panicked)
        .collect();
    assert!(
        panics.is_empty(),
        "the frontend panicked on {} workflow(s):\n{}",
        panics.len(),
        panics
            .iter()
            .map(|o| format!(
                "  {} {}: {}",
                o.repo,
                o.file,
                o.panic.as_deref().unwrap_or("?")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let others: Vec<_> = outcomes
        .iter()
        .filter(|o| o.class == Class::OtherError)
        .collect();
    assert!(
        others.is_empty(),
        "{} workflow(s) failed for a reason other than a specific rejection:\n{}",
        others.len(),
        others
            .iter()
            .map(|o| {
                format!(
                    "  {} {}:\n{}",
                    o.repo,
                    o.file,
                    o.other_errors()
                        .iter()
                        .map(|d| format!("      {d}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every rejection or ignore code the corpus run emits is declared in
/// `crates/github/SUPPORT.md`, so the support doc cannot silently drift from
/// what the code does.
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr; a test binary has no other sink"
)]
fn every_rejection_code_is_declared_in_support_md() {
    let root = corpus_root();
    if !corpus_present(&root) {
        assert!(
            !env::var("PETRI_REQUIRE_CORPUS").is_ok_and(|v| !v.is_empty()),
            "PETRI_REQUIRE_CORPUS is set, but the corpus is not fetched"
        );
        eprintln!("skipping: corpus not fetched; run scripts/corpus-fetch.sh");
        return;
    }
    let support = fs::read_to_string(root.join("../SUPPORT.md")).expect("crates/github/SUPPORT.md");

    let source = SnapshotSource::load(&root).map(|s| Arc::new(s) as Arc<dyn ActionSource>);
    let outcomes = check_all(&root, source.as_ref());
    let mut codes: BTreeSet<String> = BTreeSet::new();
    for outcome in &outcomes {
        codes.extend(outcome.unsupported_features());
        for diagnostic in &outcome.diagnostics {
            if diagnostic.code.starts_with("ignored.") {
                codes.insert(diagnostic.code.to_string());
            }
        }
    }
    // A code counts as declared only written as a code — backticked — so prose
    // that happens to contain the word does not.
    let missing: Vec<&String> = codes
        .iter()
        .filter(|code| !support.contains(&format!("`{code}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "the corpus emits codes SUPPORT.md does not declare: {missing:?}"
    );
}
