//! The runner's file contract, written by real shells: `GITHUB_ENV`,
//! `GITHUB_PATH`, `GITHUB_OUTPUT` and `GITHUB_STEP_SUMMARY` appended by bash
//! steps and read back in every position a later step can read them —
//! process env, `if:` gate, inline `${{ env.X }}`, `steps.<id>.outputs.*`.
//! Every workflow runs twice: on the host, and in a runner-image container.
//!
//! The corpus sweep stubs every `run:` to `true`, so nothing a shell does is
//! exercised there; before this battery the file contract was pinned almost
//! only through a JavaScript action, and an inline `${{ env.X }}` regression
//! hid in exactly that gap (3136d83). Behavior is pinned against GitHub's
//! runner (`src/Runner.Worker/FileCommandManager.cs`); where this runner is
//! deliberately more lenient, the test says so.
//!
//! `GITHUB_STATE` is set for a `run:` step on both runners, and on both it is
//! a write-only sink there — GitHub keeps a run step's intra-action state
//! nowhere a later step can read; this runner's `RunStep` folds none in — so
//! nothing a shell writes to it is observable. The state file's parsing rides
//! the unit tests on the shared parser instead.

mod support;

use acceptance::runs::RUNNER_IMAGE_2404;
use runtime::ir::RunStatus;
use runtime::{engine, ir};
use serde_json::json;
use support::*;

/// The same job, in a runner-image container.
fn containerized(text: &str) -> String {
    assert!(text.contains("    runs-on: ubuntu-latest\n"));
    text.replace(
        "    runs-on: ubuntu-latest\n",
        &format!("    runs-on: ubuntu-latest\n    container: {RUNNER_IMAGE_2404}\n"),
    )
}

/// Every step's recorded status, for a failing assertion to point at.
fn history_summary(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .history()
        .iter()
        .map(|h| format!("{}: {:?}", h.name, h.outcome.status))
        .collect()
}

fn assert_success(report: &RunReportPlus) {
    assert_eq!(
        report.status,
        RunStatus::Success,
        "errors: {:?}\nsteps: {:#?}\nlog: {:?}",
        report.state.errors(),
        history_summary(report),
        log_lines(report)
    );
}

fn assert_lines(report: &RunReportPlus, expected: &[&str]) {
    let lines = log_lines(report);
    for want in expected {
        assert!(
            lines.iter().any(|l| l == want),
            "missing `{want}` in {lines:?}"
        );
    }
}

fn output_of(report: &RunReportPlus, node: &str) -> ir::Value {
    report
        .state
        .run_context()
        .node(node)
        .unwrap_or_else(|| panic!("{node} ran"))
        .output
        .clone()
}

/// Every step summary the run emitted, in order.
fn step_summaries(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgress {
                ev: ir::StepEvent::Custom(value),
                ..
            } => value
                .get("github/step_summary")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

// ── 1. The env files ────────────────────────────────────────────────────────

/// One bash step appends to all four readable files; the steps after it read
/// them back every way a step can.
const ENV_FILES_WORKFLOW: &str = r###"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    env:
      DECLARED: scope-value
    steps:
      - id: writer
        run: |
          echo "PLAIN=plain-value" >> "$GITHUB_ENV"
          {
            echo "MULTI<<EOF"
            echo "line one"
            echo "line two"
            echo "EOF and more"
            echo "line four"
            echo "EOF"
          } >> "$GITHUB_ENV"
          printf 'CRLF_VAR=crlf-value\r\n' >> "$GITHUB_ENV"
          echo "WITH_EQUALS=a=b=c" >> "$GITHUB_ENV"
          echo "UTF8_VAR=héllo wörld ✓" >> "$GITHUB_ENV"
          echo "FLAG=on" >> "$GITHUB_ENV"
          echo "DECLARED=from-env-file" >> "$GITHUB_ENV"
          echo "STEP_WINS=from-env-file" >> "$GITHUB_ENV"

          mkdir -p "$GITHUB_WORKSPACE/bin-a" "$GITHUB_WORKSPACE/bin-b"
          printf '#!/bin/sh\necho tool-a-ran\n' > "$GITHUB_WORKSPACE/bin-a/probe-tool"
          printf '#!/bin/sh\necho tool-b-ran\n' > "$GITHUB_WORKSPACE/bin-b/probe-tool"
          chmod +x "$GITHUB_WORKSPACE"/bin-*/probe-tool
          echo "$GITHUB_WORKSPACE/bin-a" >> "$GITHUB_PATH"
          echo "$GITHUB_WORKSPACE/bin-b" >> "$GITHUB_PATH"

          echo "plain=out-plain" >> "$GITHUB_OUTPUT"
          {
            echo "multi<<DELIM"
            echo "first"
            echo "second"
            echo "DELIM"
          } >> "$GITHUB_OUTPUT"
          echo "flag=yes" >> "$GITHUB_OUTPUT"

          echo "## Summary" >> "$GITHUB_STEP_SUMMARY"
          echo "- from bash" >> "$GITHUB_STEP_SUMMARY"
          echo writer-done
      - id: reader
        env:
          COPIED: ${{ env.PLAIN }}
          STEP_WINS: from-step-env
          OUT_MULTI: ${{ steps.writer.outputs.multi }}
        run: |
          echo "shellvar=[$PLAIN]"
          echo "inline=[${{ env.PLAIN }}]"
          echo "stepenv=[$COPIED]"
          echo "declared=[$DECLARED]"
          echo "step-wins=[$STEP_WINS]"
          [ "$MULTI" = "$(printf 'line one\nline two\nEOF and more\nline four')" ] && echo multi-roundtrip
          [ "${{ env.MULTI }}" = "$MULTI" ] && echo inline-multi-matches
          [ "$CRLF_VAR" = "crlf-value" ] && echo crlf-stripped
          echo "equals=[$WITH_EQUALS]"
          echo "utf8=[$UTF8_VAR]"
          [ "$(printf '%s' "$PATH" | cut -d: -f1,2)" = "$GITHUB_WORKSPACE/bin-b:$GITHUB_WORKSPACE/bin-a" ] && echo path-newest-first
          probe-tool
          echo "output-plain=[${{ steps.writer.outputs.plain }}]"
          [ "$OUT_MULTI" = "$(printf 'first\nsecond')" ] && echo output-multi-roundtrip
      - id: gated
        if: env.FLAG == 'on'
        run: echo gate-saw-env
      - id: gated_output
        if: steps.writer.outputs.flag == 'yes'
        run: echo gate-saw-output
      - id: stale
        if: env.FLAG == 'off'
        run: echo never
      - id: path_again
        run: echo "$GITHUB_WORKSPACE/bin-a" >> "$GITHUB_PATH"
      - id: path_reader
        run: |
          [ "$(printf '%s' "$PATH" | cut -d: -f1,2)" = "$GITHUB_WORKSPACE/bin-a:$GITHUB_WORKSPACE/bin-b" ] && echo path-moved-to-front
          probe-tool
"###;

fn assert_env_files(report: &RunReportPlus) {
    assert_success(report);
    assert_lines(
        report,
        &[
            "writer-done",
            // `GITHUB_ENV`, read as the variable, inline, through a step-env
            // copy, and overriding a value declared in the workflow.
            "shellvar=[plain-value]",
            "inline=[plain-value]",
            "stepenv=[plain-value]",
            "declared=[from-env-file]",
            // A step's own `env:` still beats the job's accumulated file, as
            // the runner applies the action's env block last.
            "step-wins=[from-step-env]",
            // A heredoc value keeps every newline — and a body line that merely
            // starts with the delimiter does not close it.
            "multi-roundtrip",
            "inline-multi-matches",
            // A CRLF line reads as its value alone. GitHub's Linux runner
            // splits on `\n` only (its `\r\n` branch is `#if OS_WINDOWS`) and
            // would keep the `\r`; this runner strips it everywhere, as the
            // Windows runner does — a deliberate lenience, pinned.
            "crlf-stripped",
            // Only the first `=` splits.
            "equals=[a=b=c]",
            "utf8=[héllo wörld ✓]",
            // `GITHUB_PATH`: two entries from one step land newest-first, and
            // a tool placed there runs — the newest wins the name.
            "path-newest-first",
            "tool-b-ran",
            // `GITHUB_OUTPUT`, inline and through a step-env copy of a heredoc.
            "output-plain=[out-plain]",
            "output-multi-roundtrip",
            // Gates over the env and over the outputs.
            "gate-saw-env",
            "gate-saw-output",
            // Re-adding a path entry moves it to the front (GitHub removes the
            // duplicate before appending), so the older tool wins again.
            "path-moved-to-front",
            "tool-a-ran",
        ],
    );
    assert!(
        !log_lines(report).iter().any(|l| l == "never"),
        "the stale gate stays closed"
    );
    assert_eq!(status_of(report, "j/stale").as_deref(), Some("skipped"));

    // The outputs on the record itself, exactly as written.
    let output = output_of(report, "j/writer");
    assert_eq!(output["plain"], json!("out-plain"));
    assert_eq!(output["multi"], json!("first\nsecond"));
    assert_eq!(output["flag"], json!("yes"));

    // The summary, byte for byte, including its trailing newline.
    assert_eq!(
        step_summaries(report),
        vec!["## Summary\n- from bash\n".to_string()]
    );
}

#[tokio::test]
async fn shell_steps_write_and_read_the_env_files() {
    let graph = lower_ok(ENV_FILES_WORKFLOW);
    let report = run_host(graph, "shell-env-files").await;
    assert_env_files(&report);
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_steps_write_and_read_the_env_files_in_a_container() {
    if !testkit::docker_ready().await {
        return;
    }
    let graph = lower_ok(&containerized(ENV_FILES_WORKFLOW));
    let report = run_host(graph, "shell-env-files-boxed").await;
    assert_env_files(&report);
}

// ── 2. Working directories ──────────────────────────────────────────────────

/// `defaults.run.working-directory` at the workflow, overridden at the job,
/// overridden at the step; every relative path resolves against
/// `GITHUB_WORKSPACE`. The directories are made by an earlier real step —
/// which is itself the test that a script's filesystem effects reach the
/// next step. `pwd -P` on both sides keeps symlinked temp dirs honest.
const WORKING_DIRECTORY_WORKFLOW: &str = r#"
on: push
defaults:
  run:
    working-directory: wf-dir
jobs:
  inherits:
    runs-on: ubuntu-latest
    steps:
      - id: prepare
        working-directory: .
        run: |
          mkdir -p wf-dir
          echo persisted > wf-dir/marker.txt
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE" && pwd -P)" ] && echo dot-is-the-workspace
      - run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE/wf-dir" && pwd -P)" ] && echo inherits-workflow-default
          [ "$(cat marker.txt)" = persisted ] && echo marker-persisted
  overrides:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: job-dir
    steps:
      - working-directory: .
        run: mkdir -p job-dir step-dir tpl-dir sub/deeper
      - run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE/job-dir" && pwd -P)" ] && echo inherits-job-default
      - working-directory: step-dir
        run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE/step-dir" && pwd -P)" ] && echo step-overrides-job
      - working-directory: sub/deeper
        run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE/sub/deeper" && pwd -P)" ] && echo nested-relative-resolves
      - working-directory: tpl-dir
        shell: bash --noprofile --norc -euo pipefail {0}
        run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE/tpl-dir" && pwd -P)" ] && echo template-runs-in-step-dir
          ( echo "${UNSET_VARIABLE_XYZ}" ) 2>/dev/null || echo template-flags-active
      - run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE/job-dir" && pwd -P)" ] && echo job-default-again
"#;

fn assert_working_directories(report: &RunReportPlus) {
    assert_success(report);
    assert_lines(
        report,
        &[
            "dot-is-the-workspace",
            "inherits-workflow-default",
            "marker-persisted",
            "inherits-job-default",
            "step-overrides-job",
            "nested-relative-resolves",
            // A custom `{0}` template with a working directory: the script is
            // staged and run from that directory, with the template's own
            // flags (`-u` here, which the direct bash invocation lacks).
            "template-runs-in-step-dir",
            "template-flags-active",
            // A step override does not leak into the step after it.
            "job-default-again",
        ],
    );
}

/// A workflow with no defaults anywhere: the step runs in `GITHUB_WORKSPACE`.
const NO_DEFAULTS_WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: |
          [ "$(pwd -P)" = "$(cd "$GITHUB_WORKSPACE" && pwd -P)" ] && echo runs-in-the-workspace
"#;

#[tokio::test]
async fn working_directories_resolve_against_the_workspace() {
    let graph = lower_ok(WORKING_DIRECTORY_WORKFLOW);
    let report = run_host(graph, "shell-workdir").await;
    assert_working_directories(&report);

    let graph = lower_ok(NO_DEFAULTS_WORKFLOW);
    let report = run_host(graph, "shell-workdir-plain").await;
    assert_success(&report);
    assert_lines(&report, &["runs-in-the-workspace"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn working_directories_resolve_against_the_workspace_in_a_container() {
    if !testkit::docker_ready().await {
        return;
    }
    let graph = lower_ok(&containerized(WORKING_DIRECTORY_WORKFLOW));
    let report = run_host(graph, "shell-workdir-boxed").await;
    assert_working_directories(&report);

    let graph = lower_ok(&containerized(NO_DEFAULTS_WORKFLOW));
    let report = run_host(graph, "shell-workdir-plain-boxed").await;
    assert_success(&report);
    assert_lines(&report, &["runs-in-the-workspace"]);
}
