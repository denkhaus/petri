//! Custom `shell:` templates end to end: the step writes the resolved script to
//! a file and runs the template with `{0}` substituted by its path, as GitHub
//! does. Expressions and outputs work the same as with the built-in shells.

use std::process::Command;
use std::time::Duration;

use frontend::NoFiles;
use frontend_gha::load;
use github_actions::RunStep;
use runtime::executor::{MapSecrets, Retention};
use runtime::ir::{Graph, RunStatus};
use runtime::{RunOptions, Runtime};
use serde_json::json;

const WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: login
        shell: bash --norc -leo pipefail {0}
        run: |
          echo "shell=$0"
          echo "repo=${{ github.repository }}"
          echo "custom=yes" >> "$GITHUB_OUTPUT"
      - shell: /usr/bin/env bash {0}
        run: echo "env-bash=ran"
      - run: echo "saw=${{ steps.login.outputs.custom }}"
"#;

const PYTHON_WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - shell: python3 {0}
        run: |
          import os
          print("python=" + os.path.basename(__file__))
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

async fn run(label: &str, workflow: &str) -> Vec<String> {
    let run_dir = testkit::RunDir::new(label);
    let lowered = load(".github/workflows/ci.yml", workflow, &NoFiles);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = with_params(lowered.graph.expect("the workflow lowers"));
    let mut options = RunOptions::new(run_dir.path().join("run"));
    options.grace = Duration::from_secs(1);
    options.retention = Retention::Never;
    let report = Runtime::standard()
        .options(options)
        .step(RunStep)
        .secrets(MapSecrets::empty())
        .run(graph)
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    testkit::log_lines(&report)
}

fn has(lines: &[String], want: &str) {
    assert!(
        lines.iter().any(|l| l == want),
        "no line `{want}` in {lines:#?}"
    );
}

#[tokio::test]
async fn a_custom_shell_template_runs_the_script_from_a_file() {
    if !have("bash") {
        eprintln!("skipping: bash is needed");
        return;
    }
    // The run directory deliberately contains shell metacharacters. The runner
    // passes the script path through an environment variable, not command text.
    let lines = run("gha custom $shell", WORKFLOW).await;
    // The script really ran from a file, under the template's interpreter.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("shell=") && l.ends_with("/script")),
        "{lines:#?}"
    );
    // Expressions resolved into the script before it was written.
    has(&lines, "repo=example/repo");
    // `/usr/bin/env`-style templates work.
    has(&lines, "env-bash=ran");
    // GITHUB_OUTPUT flows out of a custom-shell step as usual.
    has(&lines, "saw=yes");
}

#[tokio::test]
async fn a_python_shell_template_runs_python() {
    if !have("python3") {
        eprintln!("skipping: python3 is needed");
        return;
    }
    let lines = run("gha-custom-python", PYTHON_WORKFLOW).await;
    has(&lines, "python=script");
}
