//! Acceptance §7 item 5, first half: every corpus workflow that lowers runs
//! to completion on the host executor under the stub registry, with
//! byte-identical replay, and the sweep is written to
//! `crates/fabro/corpus/RUNS.md`.

use std::{env, fs};

use fabro_acceptance::runs::{run_with_children, runs_report};
use fabro_acceptance::{corpus_root, has_corpus, lower_one, pin, workflows};

/// Corpus workflows that cannot reach exit under stubs, and why. A stub
/// produces no context, so a `for_each` whose list a real command builds has
/// nothing to expand, and the engine reports the null list as a run error —
/// Fabro's engine fails that stage the same way.
const EXPECTED_STOPS: &[(&str, &str)] = &[(
    ".fabro/workflows/code-review/code-review.fabro",
    "`finders` fans out over a list the `prepare` command builds",
)];

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
    for (file, why) in EXPECTED_STOPS {
        let result = results.iter().find(|(f, _)| f == file);
        assert!(
            result.is_some_and(|(_, r)| r.status != "success"),
            "{file} is listed as an expected stop ({why}) but reached exit or did not lower; \
             drop it from EXPECTED_STOPS"
        );
    }
    let stuck: Vec<_> = results
        .iter()
        .filter(|(f, r)| r.status != "success" && !EXPECTED_STOPS.iter().any(|(e, _)| e == f))
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
