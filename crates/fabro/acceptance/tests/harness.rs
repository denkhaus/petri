//! Acceptance §7 items 1 and 2: every corpus file lowers or is rejected
//! with a specific `unsupported.*` code, zero panics, and the report is
//! written to `crates/fabro/corpus/REPORT.md` on every run.

use std::{env, fs};

use fabro_acceptance::{Class, check_all, corpus_root, has_corpus, pin, report};

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr; a test binary has no other sink"
)]
fn every_corpus_workflow_lowers_or_is_rejected_specifically() {
    let root = corpus_root();
    if !has_corpus(&root) {
        assert!(
            !env::var("PETRI_REQUIRE_FABRO_CORPUS").is_ok_and(|v| !v.is_empty()),
            "PETRI_REQUIRE_FABRO_CORPUS is set, but the Fabro corpus is not fetched"
        );
        eprintln!("skipping: Fabro corpus not fetched; run scripts/corpus-fetch-fabro.sh");
        return;
    }
    let outcomes = check_all(&root);
    assert!(
        outcomes.len() >= 30,
        "the corpus is fetched: {} workflows",
        outcomes.len()
    );
    let markdown = report(&outcomes, &pin());
    fs::write(root.join("../REPORT.md"), &markdown).expect("write the report");

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
            .map(|o| format!("  {}: {}", o.file, o.panic.as_deref().unwrap_or("?")))
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
            .map(|o| format!(
                "  {}:\n{}",
                o.file,
                o.other_errors()
                    .iter()
                    .map(|d| format!("      {d}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The Attractor fixtures are the dialect boundary: one that uses an
    // Attractor spelling is rejected as `unsupported.attractor` (or another
    // specific code), never as a generic error; one written in the Fabro
    // subset lowers like any Fabro file.
    for outcome in outcomes.iter().filter(|o| o.is_attractor()) {
        assert!(
            matches!(
                outcome.class,
                Class::Unsupported | Class::Clean | Class::Warnings
            ),
            "{} is an Attractor fixture and must lower or be an expected rejection, got {:?}",
            outcome.file,
            outcome.class
        );
    }
}
