//! Handoff §7 test 8: the corpus bar.
//!
//! Every vendored workflow either lowers or is rejected with a specific
//! `unsupported.*` code. Zero panics, zero generic errors. The report is written to
//! `crates/github/corpus/REPORT.md` on every run so it stays current with the code.

use acceptance::{Class, check_all, report};

fn corpus_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

#[test]
fn every_corpus_workflow_lowers_or_is_rejected_specifically() {
    let root = corpus_root();
    let outcomes = check_all(&root);
    assert!(
        outcomes.len() > 100,
        "the corpus is vendored: {} workflows",
        outcomes.len()
    );

    let markdown = report(&outcomes);
    std::fs::write(root.join("REPORT.md"), &markdown).expect("write the report");

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
