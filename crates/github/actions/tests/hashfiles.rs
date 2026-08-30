//! `hashFiles(...)` end to end: an earlier step writes files, a later step's
//! `env:` and `run:` carry `${{ hashFiles(...) }}`, and the step resolves the
//! sentinel against the workspace at spawn — through the same `node` the runner
//! already requires. Skips without `node`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use frontend::NoFiles;
use frontend_gha::load;
use github_actions::RunStep;
use runtime::executor::{MapSecrets, Retention};
use runtime::ir::{Graph, RunStatus};
use runtime::{RunOptions, Runtime};
use serde_json::json;

/// SHA-256 of each matched file's SHA-256, GitHub's fold. Precomputed for the
/// fixture files below: `sha256(sha256("one"))` and
/// `sha256(sha256("one") || sha256("two"))`, paths sorted.
const HASH_A: &str = "fe8d7a873dc48961a6af334c996b2cb3ce37149d5ce9c9253952a54c6a92c1ad";
const HASH_BOTH: &str = "11914c19a28a98c57d12f3cce6c32b7944784f4b4781a706c24eb1dc284e2856";

const WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: |
          printf one > a.lock
          mkdir -p sub
          printf two > sub/b.lock
          printf other > c.txt
      - env:
          CACHE_KEY: v1-${{ hashFiles('**/*.lock') }}
        run: |
          echo "key=$CACHE_KEY"
          echo "inline=${{ hashFiles('**/*.lock') }}"
          echo "single=${{ hashFiles('a.lock') }}"
          echo "negated=${{ hashFiles('**/*.lock', '!sub/**') }}"
          echo "none=[${{ hashFiles('*.nope') }}]"
"#;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn with_params(mut graph: Graph) -> Graph {
    graph.params.insert(
        "github".into(),
        json!({ "repository": "example/repo", "event_name": "push" }),
    );
    graph
        .params
        .insert("runner".into(), json!({ "os": "Linux", "arch": "X64" }));
    graph.params.insert("vars".into(), json!({}));
    graph
}

fn runtime(dir: &Path) -> Runtime {
    let mut options = RunOptions::new(dir.join("run"));
    options.grace = Duration::from_secs(1);
    options.retention = Retention::Never;
    Runtime::standard()
        .options(options)
        .step(RunStep)
        .secrets(MapSecrets::empty())
}

#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "a test binary has no log sink; stderr carries the skip note and the diagnostics"
)]
async fn hashfiles_resolves_against_the_workspace_at_spawn() {
    if !have("node") {
        eprintln!("skipping: node is needed");
        return;
    }
    let run_dir = testkit::RunDir::new("gha-hashfiles");

    let lowered = load(".github/workflows/ci.yml", WORKFLOW, &NoFiles);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = with_params(lowered.graph.expect("the workflow lowers"));

    let report = runtime(run_dir.path())
        .run(graph)
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let lines = testkit::log_lines(&report);
    let has = |want: &str| {
        assert!(
            lines.iter().any(|l| l == want),
            "no line `{want}` in {lines:#?}"
        );
    };
    // Through `env:`, through the script text, matching GitHub's algorithm.
    has(&format!("key=v1-{HASH_BOTH}"));
    has(&format!("inline={HASH_BOTH}"));
    has(&format!("single={HASH_A}"));
    // `!` subtracts; no match is the empty string.
    has(&format!("negated={HASH_A}"));
    has("none=[]");
}
