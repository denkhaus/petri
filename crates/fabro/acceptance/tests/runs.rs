//! Acceptance §7 item 5, first half: every corpus workflow that lowers runs
//! to completion on the host executor under the stub registry, with
//! byte-identical replay, and the sweep is written to
//! `crates/fabro/corpus/RUNS.md`.

use std::{env, fs};

use fabro_acceptance::runs::{run_with_children, runs_report};
use fabro_acceptance::{corpus_root, has_corpus, lower_one, pin, workflows};

#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr; a test binary has no other sink"
)]
async fn every_lowered_corpus_workflow_runs_under_stubs() {
    let root = corpus_root();
    if !has_corpus(&root) {
        assert!(
            !env::var("PETRI_REQUIRE_FABRO_CORPUS").is_ok_and(|v| !v.is_empty()),
            "PETRI_REQUIRE_FABRO_CORPUS is set, but the Fabro corpus is not fetched"
        );
        eprintln!("skipping: Fabro corpus not fetched; run scripts/corpus-fetch-fabro.sh");
        return;
    }
    let mut results = Vec::new();
    for file in workflows(&root) {
        let (_, graph) = lower_one(&root, &file);
        let Some(artifact) = graph else {
            continue;
        };
        let result = run_with_children(artifact.graph, artifact.children, "corpus").await;
        results.push((file, result));
    }
    assert!(!results.is_empty(), "some corpus workflows lower");
    let markdown = runs_report(&results, &pin());
    fs::write(root.join("../RUNS.md"), &markdown).expect("write the sweep");
    let stuck: Vec<_> = results
        .iter()
        .filter(|(_, r)| r.status != "success")
        .collect();
    assert!(
        stuck.is_empty(),
        "{} workflow(s) did not reach exit under stubs:\n{}",
        stuck.len(),
        stuck
            .iter()
            .map(|(f, r)| format!("  {f}: {} after {:?}", r.status, r.nodes()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
