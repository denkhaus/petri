//! The shell contract, half one: workflow commands travelling the real path
//! (a shell's stdout → the line stream → the command sink → the masked log),
//! the built-in and custom shells' failure semantics, and masking through the
//! whole pipeline. Every step here is a real `run:`; the corpus sweep stubs
//! those to `true`, so nothing below had run before this battery.
//!
//! Assertions follow the runner's own source (`ActionCommandManager.cs`,
//! `ActionCommand.cs`, `OutputManager.cs`, `ScriptHandler.cs`,
//! `ScriptHandlerHelpers.cs`) and the "Workflow commands for GitHub Actions"
//! page, not petri's behaviour of the day.

mod support;

use acceptance::runs::RUNNER_IMAGE_2404;
use runtime::ir::RunStatus;
use serde_json::json;
use support::*;

fn has(lines: &[String], want: &str) {
    assert!(
        lines.iter().any(|l| l == want),
        "no line `{want}` in {lines:#?}"
    );
}

fn lacks(lines: &[String], unwanted: &str) {
    assert!(
        !lines.iter().any(|l| l == unwanted),
        "unexpected line `{unwanted}` in {lines:#?}"
    );
}

fn no_line_contains(lines: &[String], fragment: &str) {
    assert!(
        !lines.iter().any(|l| l.contains(fragment)),
        "`{fragment}` reached the log: {lines:#?}"
    );
}

/// The same workflow, every job in the runner image.
fn in_container(text: &str) -> String {
    text.replace(
        "    runs-on: ubuntu-latest\n",
        &format!("    runs-on: ubuntu-latest\n    container: {RUNNER_IMAGE_2404}\n"),
    )
}

fn record_output(report: &RunReportPlus, name: &str) -> serde_json::Value {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no record for {name}"))
        .outcome
        .output
        .clone()
}

// ── 1. Workflow commands, from a real shell ───────────────────────────────

/// One bash step emitting every `::` command the runner knows, then a step
/// reading what `set-output` left. The command sink runs in the petri process
/// whatever the executor, so the host is the representative environment.
const COMMANDS_WORKFLOW: &str = r#"
on: push
jobs:
  commands:
    runs-on: ubuntu-latest
    steps:
      - id: emit
        run: |
          echo "::add-mask::masked-value-42"
          echo "the value is masked-value-42"
          echo "::error::plain error"
          echo "::error file=app.rs,line=3,col=7,title=Oops%3A%2C::error with properties"
          echo "::warning::plain warning"
          echo "::warning title=Heads up::warning with a title"
          echo "::notice::plain notice"
          echo "::notice file=a.rs::notice with a file"
          echo "::group::Build steps"
          echo "inside the group"
          echo "::endgroup::"
          echo "::debug::debug is swallowed"
          echo "::echo::on"
          echo "::set-output name=echoed::seen"
          echo "::warning::not echoed"
          echo "::echo::off"
          echo "::set-output name=quiet::seen"
          echo "::stop-commands::pause-token-1"
          echo "::error::not-a-command"
          echo "::set-output name=ignored::never"
          echo "::pause-token-1::"
          echo "::error::commands resumed"
          echo "::warning::line one%0Aline two%0D100%25"
          echo "::set-output name=k::v-out"
          echo "::save-state name=s::v-state"
          echo "::error::on stderr" >&2
          echo "   ::notice::indented"
          echo "::unknown-command::stays"
          echo "prefix ::error::not at start"
      - run: echo "k=${{ steps.emit.outputs.k }} echoed=${{ steps.emit.outputs.echoed }} quiet=${{ steps.emit.outputs.quiet }} ignored=[${{ steps.emit.outputs.ignored }}]"
"#;

#[tokio::test]
async fn workflow_commands_travel_the_real_path() {
    let graph = lower_ok(COMMANDS_WORKFLOW);
    let report = run_host(graph, "cmd-commands").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);

    // `::add-mask::`: the command line is gone and the value is `***` from the
    // next line on.
    has(&lines, "the value is ***");
    no_line_contains(&lines, "masked-value-42");
    no_line_contains(&lines, "::add-mask::");

    // Annotations render with their level; properties (file, line, title —
    // with `%3A` / `%2C` escapes) go to the annotation, never the log line.
    has(&lines, "Error: plain error");
    has(&lines, "Error: error with properties");
    has(&lines, "Warning: plain warning");
    has(&lines, "Warning: warning with a title");
    has(&lines, "Notice: plain notice");
    has(&lines, "Notice: notice with a file");

    // Groups: the title renders, the lines inside are ordinary, `endgroup` is
    // silent.
    has(&lines, "▶ Build steps");
    has(&lines, "inside the group");
    lacks(&lines, "::group::Build steps");
    lacks(&lines, "::endgroup::");

    // `::debug::` is swallowed (no step debug logging here).
    no_line_contains(&lines, "debug is swallowed");

    // `::echo::on`: commands are also output verbatim — except the runner's
    // `OmitEcho` set (add-mask, debug, notice/warning/error). `::echo::on`
    // itself is not echoed (echo was off when it was checked); `::echo::off`
    // is (echo was still on).
    lacks(&lines, "::echo::on");
    has(&lines, "::set-output name=echoed::seen");
    lacks(&lines, "::warning::not echoed");
    has(&lines, "Warning: not echoed");
    has(&lines, "::echo::off");
    lacks(&lines, "::set-output name=quiet::seen");

    // `::stop-commands::TOKEN`: everything until `::TOKEN::` is plain text,
    // the resume line itself is output, and commands work again after it.
    lacks(&lines, "::stop-commands::pause-token-1");
    has(&lines, "::error::not-a-command");
    lacks(&lines, "Error: not-a-command");
    has(&lines, "::set-output name=ignored::never");
    has(&lines, "::pause-token-1::");
    has(&lines, "Error: commands resumed");

    // Data escapes: `%0A` → newline, `%0D` → carriage return, `%25` → `%`.
    has(&lines, "Warning: line one\nline two\r100%");

    // Legacy `set-output` reaches `steps.<id>.outputs`; the one issued under
    // `stop-commands` never applied. `save-state` is swallowed.
    has(&lines, "k=v-out echoed=seen quiet=seen ignored=[]");
    no_line_contains(&lines, "::save-state");

    // The runner reads commands from stderr too (`ScriptHandler` attaches an
    // `OutputManager` to both streams).
    has(&lines, "Error: on stderr");
    lacks(&lines, "::error::on stderr");

    // Leading whitespace is trimmed before the `::` is looked for; a `::`
    // anywhere but the start is not a command; an unknown command name is
    // plain text.
    has(&lines, "Notice: indented");
    lacks(&lines, "   ::notice::indented");
    has(&lines, "prefix ::error::not at start");
    has(&lines, "::unknown-command::stays");

    // The record carries the legacy outputs beside the exit status.
    let output = record_output(&report, "commands/emit");
    assert_eq!(output["k"], json!("v-out"));
    assert_eq!(output["exit_status"], json!(0));
}

/// `ACTIONS_ALLOW_UNSECURE_COMMANDS: true` in the job's `env:` — the runner
/// reads the opt-in from the env context, which holds the job-level env — turns
/// `set-env` and `add-path` back on, and both apply to the following steps.
const UNSECURE_ALLOWED_WORKFLOW: &str = r#"
on: push
jobs:
  allowed:
    runs-on: ubuntu-latest
    env:
      ACTIONS_ALLOW_UNSECURE_COMMANDS: true
    steps:
      - run: |
          mkdir -p "$RUNNER_TEMP/legacy-bin"
          printf '#!/bin/sh\necho legacy-tool-ran\n' > "$RUNNER_TEMP/legacy-bin/legacy-tool"
          chmod +x "$RUNNER_TEMP/legacy-bin/legacy-tool"
          echo "::set-env name=LEGACY_X::from-set-env"
          echo "::add-path::$RUNNER_TEMP/legacy-bin"
      - run: |
          echo "legacy_x=$LEGACY_X"
          legacy-tool
          case ":$PATH:" in *":$RUNNER_TEMP/legacy-bin:"*) echo path-has-legacy-bin;; esac
"#;

fn assert_unsecure_commands_applied(report: &RunReportPlus) {
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(report);
    has(&lines, "legacy_x=from-set-env");
    has(&lines, "legacy-tool-ran");
    has(&lines, "path-has-legacy-bin");
    no_line_contains(&lines, "::set-env");
    no_line_contains(&lines, "::add-path");
    no_line_contains(&lines, "is disabled");
}

#[tokio::test]
async fn set_env_and_add_path_apply_with_the_job_level_opt_in() {
    let graph = lower_ok(UNSECURE_ALLOWED_WORKFLOW);
    let report = run_host(graph, "cmd-unsecure-job").await;
    assert_unsecure_commands_applied(&report);
}

/// The same opt-in on the step's own `env:` (the other half of the runner's env
/// context).
#[tokio::test]
async fn set_env_and_add_path_apply_with_the_step_level_opt_in() {
    let text = UNSECURE_ALLOWED_WORKFLOW
        .replace(
            "    env:\n      ACTIONS_ALLOW_UNSECURE_COMMANDS: true\n",
            "",
        )
        .replace(
            "      - run: |\n          mkdir",
            "      - env:\n          ACTIONS_ALLOW_UNSECURE_COMMANDS: true\n        run: |\n          mkdir",
        );
    assert!(text.contains("        run: |\n          mkdir"), "{text}");
    let graph = lower_ok(&text);
    let report = run_host(graph, "cmd-unsecure-step").await;
    assert_unsecure_commands_applied(&report);
}

/// Without the opt-in, the runner's `set-env` / `add-path` extensions throw:
/// `TryProcessCommand` logs "Unable to process command ... successfully." and
/// the disabled message, nothing is applied, and the step fails — the process
/// itself keeps running.
const UNSECURE_DISABLED_WORKFLOW: &str = r#"
on: push
jobs:
  disabled:
    runs-on: ubuntu-latest
    steps:
      - id: legacy
        run: |
          echo "::set-env name=LEGACY_X::from-set-env"
          echo "::add-path::/nowhere/legacy-bin"
          echo still-running
      - if: always()
        run: |
          echo "legacy_x=[$LEGACY_X]"
          case ":$PATH:" in *":/nowhere/legacy-bin:"*) echo path-leaked;; esac
"#;

const DISABLED_TAIL: &str = "command is disabled. Please upgrade to using Environment Files or opt \
     into unsecure command execution by setting the `ACTIONS_ALLOW_UNSECURE_COMMANDS` \
     environment variable to `true`. For more information see: \
     https://github.blog/changelog/2020-10-01-github-actions-deprecating-set-env-and-add-path-commands/";

#[tokio::test]
async fn set_env_and_add_path_are_refused_without_the_opt_in() {
    let graph = lower_ok(UNSECURE_DISABLED_WORKFLOW);
    let report = run_host(graph, "cmd-unsecure-off").await;
    let lines = log_lines(&report);
    has(
        &lines,
        "Error: Unable to process command '::set-env name=LEGACY_X::from-set-env' successfully.",
    );
    has(&lines, &format!("Error: The `set-env` {DISABLED_TAIL}"));
    has(
        &lines,
        "Error: Unable to process command '::add-path::/nowhere/legacy-bin' successfully.",
    );
    has(&lines, &format!("Error: The `add-path` {DISABLED_TAIL}"));
    has(&lines, "still-running");
    has(&lines, "legacy_x=[]");
    lacks(&lines, "path-leaked");
}

/// The failure half of the refusal: `context.CommandResult = TaskResult.Failed`
/// fails the step even though its process exited 0.
#[tokio::test]
async fn a_refused_set_env_fails_the_step() {
    let graph = lower_ok(UNSECURE_DISABLED_WORKFLOW);
    let report = run_host(graph, "cmd-unsecure-fails").await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(
        status_of(&report, "disabled/legacy").as_deref(),
        Some("failure")
    );
}

// ── 2. Shells ─────────────────────────────────────────────────────────────

/// Built-in `bash` (`-e -o pipefail`), built-in `sh` (`-e` only), custom
/// templates, and the exit-code mapping. Later steps carry `if: always()` so
/// every case runs whatever the earlier ones did.
const SHELLS_WORKFLOW: &str = r#"
on: push
jobs:
  bash:
    runs-on: ubuntu-latest
    steps:
      - id: pipefail
        run: |
          false | cat
          echo after-pipe
      - id: errexit
        if: always()
        run: |
          echo before-false
          false
          echo after-false
      - id: lenient
        if: always()
        run: |
          set +e
          false
          echo survived-under-set-plus-e
      - id: lastexit
        if: always()
        run: |
          echo tail
          exit 3
      - id: soft
        if: always()
        continue-on-error: true
        run: exit 3
      - if: always()
        run: |
          echo "pipefail=${{ steps.pipefail.outcome }}/${{ steps.pipefail.conclusion }}"
          echo "lenient=${{ steps.lenient.outcome }}/${{ steps.lenient.conclusion }}"
          echo "soft=${{ steps.soft.outcome }}/${{ steps.soft.conclusion }}"
  sh:
    runs-on: ubuntu-latest
    steps:
      - id: nopipefail
        shell: sh
        run: |
          false | cat
          echo sh-after-pipe
      - id: errexit
        if: always()
        shell: sh
        run: |
          echo sh-before-false
          false
          echo sh-after-false
  custom:
    runs-on: ubuntu-latest
    steps:
      - id: strict
        shell: bash --noprofile --norc -eo pipefail {0}
        run: |
          false | cat
          echo strict-after-pipe
      - id: loose
        if: always()
        shell: /usr/bin/env bash {0}
        run: |
          false | cat
          echo loose-after-pipe
      - if: always()
        run: mkdir -p sub
      - if: always()
        shell: bash --noprofile --norc -eo pipefail {0}
        working-directory: sub
        run: echo "custom-pwd=$(pwd)"
"#;

fn assert_shell_semantics(report: &RunReportPlus) {
    assert_eq!(report.status, RunStatus::Failed);
    let lines = log_lines(report);
    let status = |name: &str| status_of(report, name).unwrap_or_default();

    // bash: `-o pipefail` fails the pipeline, `-e` ends the script there.
    assert_eq!(status("bash/pipefail"), "failure");
    lacks(&lines, "after-pipe");
    // bash: `-e` stops at the first failing command.
    assert_eq!(status("bash/errexit"), "failure");
    has(&lines, "before-false");
    lacks(&lines, "after-false");
    // The script may turn `-e` back off.
    assert_eq!(status("bash/lenient"), "success");
    has(&lines, "survived-under-set-plus-e");
    // A non-zero exit fails the step; the code is in the record.
    assert_eq!(status("bash/lastexit"), "failure");
    has(&lines, "tail");
    assert_eq!(
        record_output(report, "bash/lastexit")["exit_status"],
        json!(3)
    );
    // `continue-on-error`: outcome is what happened, conclusion is what the
    // job sees.
    assert_eq!(status("bash/soft"), "partial_success");
    assert_eq!(record_output(report, "bash/soft")["exit_status"], json!(3));
    has(&lines, "pipefail=failure/failure");
    has(&lines, "lenient=success/success");
    has(&lines, "soft=failure/success");

    // sh: `-e` but no `pipefail` (`sh -e {0}`), so a failing pipeline stage
    // is not a failure while a failing command is.
    assert_eq!(status("sh/nopipefail"), "success");
    has(&lines, "sh-after-pipe");
    assert_eq!(status("sh/errexit"), "failure");
    has(&lines, "sh-before-false");
    lacks(&lines, "sh-after-false");

    // Custom templates run as written: the strict one fails the pipeline, the
    // bare `/usr/bin/env bash {0}` does not; `working-directory` applies.
    assert_eq!(status("custom/strict"), "failure");
    lacks(&lines, "strict-after-pipe");
    assert_eq!(status("custom/loose"), "success");
    has(&lines, "loose-after-pipe");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("custom-pwd=") && l.ends_with("/repo/sub")),
        "{lines:#?}"
    );
}

#[tokio::test]
async fn shells_fail_the_way_github_documents() {
    let graph = lower_ok(SHELLS_WORKFLOW);
    let report = run_host(graph, "cmd-shells").await;
    assert_shell_semantics(&report);
}

#[tokio::test(flavor = "multi_thread")]
async fn shells_fail_the_way_github_documents_in_a_container() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let graph = lower_ok(&in_container(SHELLS_WORKFLOW));
    let report = run_host(graph, "cmd-shells-boxed").await;
    assert_shell_semantics(&report);
}

/// Built-in `shell: python` is the runner's `python {0}` template: the script
/// runs from a file under `python`, in the workspace, with the runner files.
const PYTHON_WORKFLOW: &str = r#"
on: push
jobs:
  py:
    runs-on: ubuntu-latest
    steps:
      - id: probe
        shell: python
        run: |
          import os, sys
          print("python-major=%d" % sys.version_info[0])
          print("cwd-ok=%s" % (os.getcwd() == os.environ["GITHUB_WORKSPACE"]))
          with open(os.environ["GITHUB_OUTPUT"], "a") as f:
              f.write("from_python=yes\n")
      - id: fails
        if: always()
        shell: python
        run: |
          import sys
          sys.exit(2)
      - if: always()
        run: echo "from_python=${{ steps.probe.outputs.from_python }} fails=${{ steps.fails.outcome }}"
"#;

fn assert_python_ran(report: &RunReportPlus) {
    assert_eq!(report.status, RunStatus::Failed);
    let lines = log_lines(report);
    has(&lines, "python-major=3");
    has(&lines, "cwd-ok=True");
    has(&lines, "from_python=yes fails=failure");
    assert_eq!(
        record_output(report, "py/fails")["exit_status"],
        json!(2),
        "python's exit code is the step's"
    );
}

#[tokio::test]
async fn the_python_shell_runs_python() {
    // GitHub's template is `python {0}`, not `python3`.
    if !is_tool_ready("python") {
        return;
    }
    let graph = lower_ok(PYTHON_WORKFLOW);
    let report = run_host(graph, "cmd-python").await;
    assert_python_ran(&report);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_python_shell_runs_python_in_a_container() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let graph = lower_ok(&in_container(PYTHON_WORKFLOW));
    let report = run_host(graph, "cmd-python-boxed").await;
    assert_python_ran(&report);
}

/// Built-in `shell: pwsh` is `pwsh -command ". '{0}'"`. The runner image has
/// no PowerShell, so this runs on the host and only where `pwsh` exists.
#[tokio::test]
async fn the_pwsh_shell_runs_powershell() {
    if !is_tool_ready("pwsh") {
        return;
    }
    let text = r#"
on: push
jobs:
  ps:
    runs-on: ubuntu-latest
    steps:
      - shell: pwsh
        run: Write-Output "pwsh-major=$($PSVersionTable.PSVersion.Major)"
      - id: fails
        shell: pwsh
        run: exit 4
      - if: always()
        run: echo "fails=${{ steps.fails.outcome }}"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "cmd-pwsh").await;
    assert_eq!(report.status, RunStatus::Failed);
    let lines = log_lines(&report);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("pwsh-major=") && l != "pwsh-major="),
        "{lines:#?}"
    );
    has(&lines, "fails=failure");
}

// ── 3. Masking through the pipeline ───────────────────────────────────────

const SINGLE_SECRET: &str = "s1ngle-secret-value";
const MULTI_SECRET: &str = "multi-secret-line-A\nmulti-secret-line-B";

/// A configured secret and a runtime `::add-mask::`, one-line and multi-line,
/// printed by bash every way a script prints: plainly, inside a line, inside a
/// `::warning::`, on stderr.
const MASKING_WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          SINGLE: ${{ secrets.SINGLE_SECRET }}
          MULTI: ${{ secrets.MULTI_SECRET }}
        run: |
          echo "single=$SINGLE"
          echo "multi=$MULTI"
          printf 'inline %s here\n' "$SINGLE"
          echo "::warning::leaked $SINGLE"
          echo "stderr $SINGLE" >&2
          echo "::add-mask::runtime-masked-value"
          echo "::add-mask::runtime-line-one%0Aruntime-line-two"
          echo "runtime=runtime-masked-value"
          printf 'runtime-line-one\nruntime-line-two\n'
          echo "::warning::warned runtime-masked-value"
          echo "::notice::joined runtime-line-one%0Aruntime-line-two"
          echo "stderr runtime-masked-value" >&2
"#;

fn assert_nothing_leaked(report: &RunReportPlus) {
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(report);
    for raw in [
        SINGLE_SECRET,
        "multi-secret-line-A",
        "multi-secret-line-B",
        "runtime-masked-value",
        "runtime-line-one",
        "runtime-line-two",
    ] {
        no_line_contains(&lines, raw);
    }
    has(&lines, "single=***");
    has(&lines, "multi=***");
    has(&lines, "inline *** here");
    has(&lines, "Warning: leaked ***");
    has(&lines, "stderr ***");
    has(&lines, "runtime=***");
    has(&lines, "Warning: warned ***");
    // The multi-line value is masked whole where it appears whole …
    has(&lines, "Notice: joined ***");
    // … and per line where the shell split it: the second line of the secret
    // and both lines of the runtime mask come out as bare `***`.
    assert!(
        lines.iter().filter(|l| l.as_str() == "***").count() >= 3,
        "{lines:#?}"
    );
}

#[tokio::test]
async fn secrets_and_runtime_masks_never_reach_the_log() {
    let graph = lower_ok(MASKING_WORKFLOW);
    let report = run_host_with_secrets(graph, "cmd-masking", &[
        ("SINGLE_SECRET", SINGLE_SECRET),
        ("MULTI_SECRET", MULTI_SECRET),
    ])
    .await;
    assert_nothing_leaked(&report);
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_and_runtime_masks_never_reach_the_log_in_a_container() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let graph = lower_ok(&in_container(MASKING_WORKFLOW));
    let report = run_host_with_secrets(graph, "cmd-masking-boxed", &[
        ("SINGLE_SECRET", SINGLE_SECRET),
        ("MULTI_SECRET", MULTI_SECRET),
    ])
    .await;
    assert_nothing_leaked(&report);
}
