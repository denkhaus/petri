//! Handoff §7 tests 3–6, the behavioural half: the status truth table,
//! matrices, composites and job outputs, lowered by the real GHA frontend and
//! run on the standard runtime. The pure lowering half stays with the frontend,
//! in `crates/github/frontend/tests/lowering.rs`.

mod support;

use acceptance::runs::RUNNER_IMAGE_2404;
use runtime::ir::RunStatus;
use runtime::{engine, frontend, ir};
use serde_json::json;
use support::*;

// ── §7 test 3: the status-function truth table ────────────────────────────

/// One job: `a` ends in each status in turn; `b`..`e` are gated on each status
/// function. Which of them ran is the truth table.
fn status_table_workflow(a_step: &str) -> String {
    format!(
        r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: a
{a_step}
      - id: b
        run: echo b
      - id: c
        if: failure()
        run: echo c
      - id: d
        if: always()
        run: echo d
      - id: e
        if: cancelled()
        run: echo e
      - id: f
        if: success() || failure()
        run: echo f
"
    )
}

#[tokio::test]
async fn status_functions_truth_table() {
    // (how `a` ends, which of b c d e f run)
    let cases: &[(&str, &str, &[&str])] = &[
        ("success", "        run: exit 0", &["b", "d", "f"]),
        ("failure", "        run: exit 1", &["c", "d", "f"]),
        // continue-on-error: a PartialSuccess is success-like for the job.
        (
            "partial_success",
            "        run: exit 1\n        continue-on-error: true",
            &["b", "d", "f"],
        ),
        ("skipped", "        if: false\n        run: exit 0", &[
            "b", "d", "f",
        ]),
    ];
    for (label, a_step, expected) in cases {
        let text = status_table_workflow(a_step);
        let graph = lower_ok(&text);
        let report = run_host(graph, &format!("truth-{label}")).await;
        let ran: Vec<String> = ["b", "c", "d", "e", "f"]
            .iter()
            .filter(|s| started(&report).iter().any(|n| n == &format!("j/{s}")))
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            ran,
            expected.to_vec(),
            "after `a` {label}: ran {ran:?}, expected {expected:?}"
        );
        let a_status = status_of(&report, "j/a").unwrap();
        assert_eq!(a_status, *label);
    }
}

/// The cancelled row of the truth table. After a cancel lands mid-step, the
/// `always()` and `cancelled()` steps run for real, the un-gated steps between
/// record `Cancelled`, and the run still reports `Cancelled`.
#[tokio::test]
async fn cancelled_steps_run_after_a_cancel() {
    let text = status_table_workflow("        run: echo ready && sleep 30");
    let graph = lower_ok(&text);
    let (report, ()) = run_host_then_cancel(graph, "truth-cancelled", "j/a").await;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "j/a").as_deref(), Some("cancelled"));

    let ran: Vec<String> = ["b", "c", "d", "e", "f"]
        .iter()
        .filter(|s| started(&report).iter().any(|n| n == &format!("j/{s}")))
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        ran,
        vec!["d", "e"],
        "`always()` and `cancelled()` run; nothing else does"
    );
    for step in ["j/b", "j/c", "j/f"] {
        assert_eq!(
            status_of(&report, step).as_deref(),
            Some("cancelled"),
            "{step} records why it did not run"
        );
    }
}

/// A not-yet-started job with `if: always()` runs after the run is cancelled —
/// GitHub's observed behavior, pinned (resolved decision 5) — and its interior
/// steps run normally. An un-gated dependent job does not start.
#[tokio::test]
async fn an_always_job_runs_after_a_run_cancel() {
    let text = r"
on: push
jobs:
  main:
    runs-on: ubuntu-latest
    steps:
      - id: work
        run: echo ready && sleep 30
  cleanup:
    needs: main
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo cleaning
      - run: echo swept
  dependent:
    needs: main
    runs-on: ubuntu-latest
    steps:
      - run: echo never
";
    let graph = lower_ok(text);
    let (report, ()) = run_host_then_cancel(graph, "always-job", "main/work").await;
    assert_eq!(report.status, RunStatus::Cancelled);

    let ran = started(&report);
    assert!(
        ran.iter().any(|n| n == "cleanup/step-1") && ran.iter().any(|n| n == "cleanup/step-2"),
        "the always() job's interior steps run normally: {ran:?}"
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"cleaning".to_string()), "{lines:?}");
    assert!(lines.contains(&"swept".to_string()), "{lines:?}");

    assert!(
        !ran.iter().any(|n| n == "dependent/step-1"),
        "the un-gated dependent does not start: {ran:?}"
    );
    assert_eq!(
        status_of(&report, "dependent/step-1").as_deref(),
        Some("cancelled")
    );
}

/// A cancel that lands with no step of the job cancelled: the cleanup job's
/// first step is gated on `cancelled()`, and no earlier step of that job
/// carries a `Cancelled` record — only the `scope_cancelled` static can make
/// the gate true. The job itself opts in with `if: always()`; a dependent with
/// no such gate never starts at all (GitHub cancels a queued job), and its
/// steps record `Cancelled` rather than running.
#[tokio::test]
async fn a_cancelled_step_fires_via_scope_cancelled() {
    let text = r"
on: push
jobs:
  w:
    runs-on: ubuntu-latest
    steps:
      - run: echo ready && sleep 30
  c:
    needs: w
    if: always()
    runs-on: ubuntu-latest
    steps:
      - id: witness
        if: cancelled()
        run: echo witness-ran
  plain:
    needs: w
    steps:
      - id: never
        if: cancelled()
        run: echo never-ran
    runs-on: ubuntu-latest
";
    let graph = lower_ok(text);
    let (report, ()) = run_host_then_cancel(graph, "between-steps", "w/step-1").await;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        log_lines(&report).contains(&"witness-ran".to_string()),
        "the first step of the job has no cancelled earlier step; scope_cancelled admits it: {:?}",
        log_lines(&report)
    );
    assert!(
        !log_lines(&report).contains(&"never-ran".to_string()),
        "a job cancelled before it started runs nothing, its cancelled() steps included"
    );
    assert_eq!(
        status_of(&report, "plain/never").as_deref(),
        Some("cancelled")
    );
}

/// Job summaries survive a cancel: a job with cleanup steps but no job-level
/// opt-in still runs its `done`, so a dependent `always()` job reads
/// `needs.J.result == "cancelled"` and the job's outputs.
#[tokio::test]
async fn job_summaries_survive_a_cancel() {
    let text = r#"
on: push
jobs:
  main:
    runs-on: ubuntu-latest
    outputs:
      artifact: ${{ steps.produce.outputs.artifact }}
    steps:
      - id: produce
        run: echo "artifact=app.tar" >> "$GITHUB_OUTPUT"
      - id: slow
        run: echo ready && sleep 30
      - id: tidy
        if: always()
        run: echo tidying
  report:
    needs: main
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo "saw ${{ needs.main.result }} with ${{ needs.main.outputs.artifact }}"
"#;
    let graph = lower_ok(text);
    let (report, ()) = run_host_then_cancel(graph, "summaries", "main/slow").await;
    assert_eq!(report.status, RunStatus::Cancelled);

    assert!(
        started(&report).iter().any(|n| n == "main/tidy"),
        "the always() cleanup step ran"
    );
    assert_eq!(
        report
            .state
            .run_context()
            .node("main/done")
            .expect("done ran")
            .output["result"],
        json!("cancelled"),
        "the summary says what happened"
    );
    assert!(
        log_lines(&report).contains(&"saw cancelled with app.tar".to_string()),
        "the dependent reads the result and the outputs: {:?}",
        log_lines(&report)
    );
}

/// Composite cleanup: a local composite with `if: always()` (and one with
/// `cancelled()`) runs its inlined steps after a cancel, while an un-gated
/// composite's inlined steps record `Cancelled`.
#[tokio::test]
async fn composite_cleanup_runs_after_a_cancel() {
    let sweeper = r"
runs:
  using: composite
  steps:
    - shell: bash
      run: echo sweeping-1
    - shell: bash
      run: echo sweeping-2
";
    let echoer = r"
runs:
  using: composite
  steps:
    - shell: bash
      run: echo plain-ran
";
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: slow
        run: echo ready && sleep 30
      - id: plain
        uses: ./.github/actions/echoer
      - id: sweep
        if: always()
        uses: ./.github/actions/sweeper
      - id: sweep2
        if: cancelled()
        uses: ./.github/actions/echoer
";
    let files = files(&[
        (".github/actions/sweeper/action.yml", sweeper),
        (".github/actions/echoer/action.yml", echoer),
    ]);
    let graph = lower_ok_with(text, &files);
    let (report, ()) = run_host_then_cancel(graph, "composite-cleanup", "j/slow").await;
    assert_eq!(report.status, RunStatus::Cancelled);

    let lines = log_lines(&report);
    assert!(lines.contains(&"sweeping-1".to_string()), "{lines:?}");
    assert!(lines.contains(&"sweeping-2".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"plain-ran".to_string()),
        "the cancelled() composite's steps ran too: {lines:?}"
    );
    assert!(
        !started(&report).iter().any(|n| n == "j/plain/step-1"),
        "the un-gated composite never starts"
    );
    assert_eq!(
        status_of(&report, "j/plain/step-1").as_deref(),
        Some("cancelled")
    );
}

/// `fail_fast` plus `max-parallel`: a leg still deferred when the splice scope
/// was cancelled never begins — GitHub cancels a queued `fail-fast` leg before
/// any of its steps, `if: cancelled()` ones included. Every step of the leg
/// records `Cancelled`.
#[tokio::test]
async fn a_deferred_leg_never_starts_under_fail_fast() {
    let text = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: true
      max-parallel: 1
      matrix:
        n: [1, 2]
    steps:
      - id: recover
        if: cancelled()
        run: echo "recover-${{ matrix.n }}"
      - id: work
        run: |
          if [ "${{ matrix.n }}" = "1" ]; then exit 1; fi
          echo "work-${{ matrix.n }}"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "fail-fast-deferred").await;
    assert_eq!(report.status, RunStatus::Failed);

    let lines = log_lines(&report);
    assert!(
        !lines.contains(&"recover-1".to_string()),
        "the failing leg's cancelled() step was evaluated before the cancel and skipped"
    );
    assert_eq!(
        status_of(&report, "test/recover#0").as_deref(),
        Some("skipped")
    );
    assert!(
        !lines.contains(&"recover-2".to_string()),
        "the deferred leg was cancelled before it started; nothing of it runs: {lines:?}"
    );
    for step in ["test/start#1", "test/recover#1", "test/work#1"] {
        assert_eq!(
            status_of(&report, step).as_deref(),
            Some("cancelled"),
            "{step} records why it did not run"
        );
    }
    assert!(
        !lines.contains(&"work-2".to_string()),
        "it never actually ran"
    );
}

/// Job-level status functions read the needed jobs' summaries.
#[tokio::test]
async fn job_level_status_functions_read_needs() {
    let text = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  deploy:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo deploy
  notify:
    needs: build
    if: failure()
    runs-on: ubuntu-latest
    steps:
      - run: echo notify
  cleanup:
    needs: [build, deploy]
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo cleanup
  unrelated_if:
    needs: build
    if: github.event_name == 'push'
    runs-on: ubuntu-latest
    steps:
      - run: echo never
";
    let graph = lower_ok(text).with_param("github", json!({"event_name": "push"}));
    let report = run_host(graph, "job-status").await;
    assert_eq!(report.status, RunStatus::Failed);
    let ran = started(&report);
    assert!(
        !ran.iter()
            .any(|n| n.starts_with("deploy/") && n != "deploy/start" && n != "deploy/done"),
        "deploy skipped: {ran:?}"
    );
    assert!(
        ran.iter().any(|n| n == "notify/step-1"),
        "notify ran on failure(): {ran:?}"
    );
    assert!(
        ran.iter().any(|n| n == "cleanup/step-1"),
        "cleanup ran on always(): {ran:?}"
    );
    assert!(
        !ran.iter().any(|n| n == "unrelated_if/step-1"),
        "an `if` with no status function still gets success() applied: {ran:?}"
    );
    // needs.build.result is visible downstream.
    assert_eq!(
        report
            .state
            .run_context()
            .node("build/done")
            .unwrap()
            .output["result"],
        json!("failure")
    );
}

// ── §7 test 4: matrix ─────────────────────────────────────────────────────

#[tokio::test]
async fn static_matrix_expands_and_collects() {
    let text = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [linux, mac]
        node: [18, 20]
        exclude:
          - os: mac
            node: 18
        include:
          - os: linux
            node: 20
            coverage: true
    steps:
      - run: echo "${{ matrix.os }}-${{ matrix.node }}-${{ matrix.coverage }}"
  after:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - run: echo "all legs done"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "matrix-static").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"linux-18-".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"linux-20-true".to_string()),
        "include added coverage: {lines:?}"
    );
    assert!(lines.contains(&"mac-20-".to_string()));
    assert!(
        !lines.iter().any(|l| l == "mac-18-"),
        "excluded leg did not run"
    );
    assert_eq!(
        lines.iter().filter(|l| l.contains('-')).count(),
        3,
        "three legs"
    );
    assert_eq!(
        started(&report)
            .iter()
            .filter(|n| n.as_str() == "after/step-1")
            .count(),
        1
    );
    assert_eq!(
        report.state.run_context().node("test/done").unwrap().output["result"],
        json!("success")
    );
}

// ── Reusable workflows ────────────────────────────────────────────────────

/// A local workflow call, end to end: inputs bind, the callee's jobs run in
/// order under the call's prefix, outputs map through `workflow_call.outputs`,
/// and the caller's dependent reads them as `needs.<call>.outputs.*`.
#[tokio::test]
async fn a_workflow_call_runs_end_to_end() {
    let callee = r#"
on:
  workflow_call:
    inputs:
      version:
        type: string
        required: true
    outputs:
      artifact:
        value: ${{ jobs.build.outputs.artifact }}
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      artifact: ${{ steps.pack.outputs.name }}
    steps:
      - id: pack
        run: echo "name=app-${{ inputs.version }}" >> "$GITHUB_OUTPUT"
  check:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo "checking ${{ needs.build.outputs.artifact }}"
"#;
    let caller = r#"
on: push
jobs:
  release:
    uses: ./.github/workflows/build.yml
    with:
      version: "1.2"
  announce:
    needs: release
    runs-on: ubuntu-latest
    steps:
      - run: echo "released ${{ needs.release.outputs.artifact }} (${{ needs.release.result }})"
"#;
    let files = files(&[(".github/workflows/build.yml", callee)]);
    let graph = lower_ok_with(caller, &files);
    let report = run_host(graph, "call-basic").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"checking app-1.2".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"released app-1.2 (success)".to_string()),
        "{lines:?}"
    );
}

/// A skipped call skips the whole callee — even its `always()` jobs — and the
/// caller's dependents read `skipped`, exactly as GitHub concludes.
#[tokio::test]
async fn a_skipped_call_skips_the_callee() {
    let callee = r#"
on:
  workflow_call: {}
jobs:
  work:
    runs-on: ubuntu-latest
    steps:
      - run: echo "callee ran"
  cleanup:
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo "callee cleanup ran"
"#;
    let caller = r#"
on: push
jobs:
  gated:
    if: github.ref == 'refs/heads/never'
    uses: ./.github/workflows/inner.yml
  after:
    needs: gated
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo "call result=${{ needs.gated.result }}"
"#;
    let files = files(&[(".github/workflows/inner.yml", callee)]);
    let graph = lower_ok_with(caller, &files);
    let report = run_host(graph, "call-skipped").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(
        !lines.iter().any(|l| l.contains("callee")),
        "nothing of the callee runs: {lines:?}"
    );
    assert!(
        lines.contains(&"call result=skipped".to_string()),
        "{lines:?}"
    );
}

/// A matrix on the call fans the whole callee out per leg, `with:` evaluated
/// per leg from the caller's matrix.
#[tokio::test]
async fn a_matrix_call_fans_out_the_callee() {
    let callee = r#"
on:
  workflow_call:
    inputs:
      version:
        type: string
        required: true
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo "building ${{ inputs.version }}"
"#;
    let caller = r#"
on: push
jobs:
  fan:
    strategy:
      matrix:
        v: ["1", "2"]
    uses: ./.github/workflows/build.yml
    with:
      version: ${{ matrix.v }}
  after:
    needs: fan
    runs-on: ubuntu-latest
    steps:
      - run: echo "fanned=${{ needs.fan.result }}"
"#;
    let files = files(&[(".github/workflows/build.yml", callee)]);
    let graph = lower_ok_with(caller, &files);
    let report = run_host(graph, "call-matrix").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"building 1".to_string()), "{lines:?}");
    assert!(lines.contains(&"building 2".to_string()), "{lines:?}");
    assert!(lines.contains(&"fanned=success".to_string()), "{lines:?}");
}

/// Secrets cross the call boundary by name only: an explicit `secrets:` block
/// renames at lowering, and the value still enters nowhere but the process.
#[tokio::test]
async fn call_secrets_map_through_the_boundary() {
    let callee = r#"
on:
  workflow_call:
    secrets:
      deploy_key:
        required: true
jobs:
  use:
    runs-on: ubuntu-latest
    steps:
      - run: echo "len=${#KEY}"
        env:
          KEY: ${{ secrets.deploy_key }}
"#;
    let caller = r"
on: push
jobs:
  ship:
    uses: ./.github/workflows/inner.yml
    secrets:
      deploy_key: ${{ secrets.REAL_KEY }}
";
    let files = files(&[(".github/workflows/inner.yml", callee)]);
    let graph = lower_ok_with(caller, &files);
    let encoded = serde_json::to_string(&graph).unwrap();
    assert!(
        !encoded.contains("REAL_KEY_VALUE"),
        "no secret value in the graph"
    );
    let report =
        run_host_with_secrets(graph, "call-secrets", &[("REAL_KEY", "s3cret-value")]).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"len=12".to_string()), "{lines:?}");
}

/// Dispatch inputs come from the run's parameters through the typed model:
/// provided values win, declared defaults fill the gaps, booleans are real.
#[tokio::test]
async fn dispatch_inputs_bind_from_run_parameters() {
    let text = r#"
on:
  workflow_dispatch:
    inputs:
      target:
        type: string
        default: nowhere
      dry-run:
        type: boolean
        default: true
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - if: ${{ !inputs.dry-run }}
        run: echo "deploying to ${{ inputs.target }}"
      - if: ${{ inputs.dry-run }}
        run: echo "dry run for ${{ inputs.target }}"
"#;
    let mut graph = lower_ok(text);
    graph.params.insert(
        "github".into(),
        serde_json::json!({
            "sha": "0123456789abcdef", "ref": "refs/heads/main", "ref_name": "main",
            "repository": "example/repo", "actor": "tester",
            "event_name": "workflow_dispatch", "run_id": "1", "run_number": "1",
            "event": { "inputs": { "target": "staging", "dry-run": "false" } },
        }),
    );
    let report = run_host(graph, "dispatch-inputs").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(
        lines.contains(&"deploying to staging".to_string()),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("dry run")),
        "the boolean coerced: {lines:?}"
    );
}

/// `runs-on: ${{ matrix.os }}`: the labels resolve at lowering per leg, and the
/// legs the engine expands at run time are the same legs — the job runs.
#[tokio::test]
async fn expression_runs_on_matrix_runs_end_to_end() {
    let text = r#"
on: push
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-latest, ubuntu-24.04]
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo "on-${{ matrix.os }}"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "matrix-runs-on").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"on-ubuntu-latest".to_string()), "{lines:?}");
    assert!(lines.contains(&"on-ubuntu-24.04".to_string()), "{lines:?}");
}

/// An expression-valued matrix stays unevaluated in HIR and expands at run
/// time.
#[tokio::test]
async fn expression_matrix_expands_at_runtime() {
    let text = r#"
on: push
jobs:
  plan:
    runs-on: ubuntu-latest
    outputs:
      targets: ${{ steps.p.outputs.targets }}
    steps:
      - id: p
        run: echo 'targets=["alpha","beta","gamma"]' >> "$GITHUB_OUTPUT"
  build:
    needs: plan
    runs-on: ubuntu-latest
    strategy:
      matrix:
        target: ${{ fromJSON(needs.plan.outputs.targets) }}
    steps:
      - run: echo "building ${{ matrix.target }}"
"#;
    let graph = lower_ok(text);
    // Nothing about the matrix was evaluated at lowering time.
    let start = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/start")
        .unwrap();
    assert!(start.expand.is_some());
    let printed = frontend::print_graph(&graph);
    // GitHub's matrix rule is a composition of engine combinators, not a builtin.
    assert!(
        printed.contains("extend_where(reject_where(cartesian("),
        "{printed}"
    );
    assert!(printed.contains("from_json"), "{printed}");
    assert!(
        !printed.contains("matrix_combinations"),
        "no GitHub-specific builtin exists"
    );

    let report = run_host(graph, "matrix-dynamic").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    for t in ["alpha", "beta", "gamma"] {
        assert!(lines.contains(&format!("building {t}")), "{lines:?}");
    }
}

/// `fail-fast` maps onto the splice's cancel scope.
#[tokio::test]
async fn matrix_fail_fast_cancels_siblings() {
    let text = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: true
      matrix:
        n: [1, 2, 3]
    steps:
      - run: |
          if [ "${{ matrix.n }}" = "1" ]; then exit 1; fi
          sleep 5
"#;
    let graph = lower_ok(text);
    let start = graph.nodes.iter().find(|n| n.name == "test/start").unwrap();
    assert!(matches!(
        start.expand,
        Some(ir::Expansion::ForEach {
            fail_fast: true,
            ..
        })
    ));
    let report = run_host(graph, "matrix-fail-fast").await;
    assert_eq!(report.status, RunStatus::Failed);
    // The splice's own cancel scope was cancelled — a Core event in the log — and a
    // sibling leg ended cancelled rather than sleeping out its five seconds.
    let scope_cancels = report
        .state
        .log
        .events()
        .filter(|e| matches!(e, engine::Event::CancelRequested { scope } if *scope != ir::CancelScopeId::ROOT))
        .count();
    assert!(scope_cancels >= 1, "fail-fast cancelled the splice scope");
    assert!(
        report
            .state
            .history()
            .iter()
            .any(|r| r.name.starts_with("test/step-1#") && r.outcome.status.tag() == "cancelled"),
        "a sibling leg was cancelled: {:?}",
        report
            .state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag()))
            .collect::<Vec<_>>()
    );
}

// ── §7 test 5: composite actions ──────────────────────────────────────────

#[tokio::test]
async fn composite_actions_inline_with_inputs_and_outputs() {
    let action = r#"
name: greet
inputs:
  who:
    required: true
  greeting:
    default: hello
outputs:
  message:
    value: ${{ steps.say.outputs.msg }}
runs:
  using: composite
  steps:
    - id: say
      shell: bash
      run: |
        echo "${{ inputs.greeting }}, ${{ inputs.who }}"
        echo "msg=${{ inputs.greeting }}-${{ inputs.who }}" >> "$GITHUB_OUTPUT"
    - uses: ./.github/actions/inner
      with:
        depth: two
"#;
    let inner = r#"
runs:
  using: composite
  steps:
    - shell: bash
      run: echo "inner at ${{ inputs.depth }}"
inputs:
  depth:
    default: one
"#;
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: g
        uses: ./.github/actions/greet
        with:
          who: world
      - run: echo "got ${{ steps.g.outputs.message }}"
"#;
    let files = files(&[
        (".github/actions/greet/action.yml", action),
        (".github/actions/inner/action.yml", inner),
    ]);
    let graph = lower_ok_with(text, &files);
    let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.contains(&"j/g/say"),
        "inlined under the caller: {names:?}"
    );
    assert!(
        names.contains(&"j/g/step-2/step-1"),
        "nested composite inlined: {names:?}"
    );

    let report = run_host(graph, "composite").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"hello, world".to_string()), "{lines:?}");
    assert!(lines.contains(&"inner at two".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"got hello-world".to_string()),
        "composite outputs reach the caller: {lines:?}"
    );
}

/// Job outputs flow to dependents through `done`.
#[tokio::test]
async fn job_outputs_reach_dependents() {
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      artifact: ${{ steps.compile.outputs.artifact }}
    steps:
      - id: compile
        run: echo "artifact=app.tar" >> "$GITHUB_OUTPUT"
  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo "testing ${{ needs.build.outputs.artifact }}"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "job-outputs").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(
        log_lines(&report).contains(&"testing app.tar".to_string()),
        "{:?}",
        log_lines(&report)
    );
}

// ── Lazy step conditions: what only the step can resolve ──────────────────

/// The React sizebot shape: a step gated on `hashFiles(...) != ''`, resolved
/// against the workspace at spawn. Skips without `node`, which the hash helper
/// runs on.
#[tokio::test]
async fn hashfiles_conditions_read_the_workspace() {
    if !is_tool_ready("node") {
        return;
    }
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: printf lock > yarn.lock
      - id: with_lock
        if: hashFiles('yarn.lock') != ''
        run: echo have-lock
      - id: without_lock
        if: hashFiles('nope.lock') != ''
        run: echo no-lock
";
    let graph = lower_ok(text);
    let report = run_host(graph, "hashfiles-if").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"have-lock".to_string()), "{lines:?}");
    assert!(!lines.contains(&"no-lock".to_string()), "{lines:?}");
    assert_eq!(
        status_of(&report, "j/without_lock").as_deref(),
        Some("skipped")
    );
}

/// A condition over `env.*` sees what earlier steps appended through
/// `GITHUB_ENV` — GitHub evaluates step conditions on the runner with that
/// environment — while values declared in the workflow still resolve through
/// the scope env.
#[tokio::test]
async fn conditions_read_the_step_environment() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    env:
      FLAG: "off"
      DECLARED: "yes"
    steps:
      - run: echo "FLAG=on" >> "$GITHUB_ENV"
      - id: appended
        if: env.FLAG == 'on'
        run: echo saw-appended
      - id: stale
        if: env.FLAG == 'off'
        run: echo saw-stale
      - id: declared
        if: env.DECLARED == 'yes'
        run: echo saw-declared
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "env-if").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"saw-appended".to_string()), "{lines:?}");
    assert!(lines.contains(&"saw-declared".to_string()), "{lines:?}");
    assert!(!lines.contains(&"saw-stale".to_string()), "{lines:?}");
    assert_eq!(status_of(&report, "j/stale").as_deref(), Some("skipped"));
}

/// The workflow behind [`step_config_reads_the_step_environment`]: an append
/// through `GITHUB_ENV`, then a step whose config reads it every way step
/// config can.
const STEP_ENV_CONFIG_WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    env:
      DECLARED: scope-value
    steps:
      - run: echo "PROBE=from_env_file" >> "$GITHUB_ENV"
      - env:
          COPIED: ${{ env.PROBE }}
        run: |
          echo "inline=[${{ env.PROBE }}]"
          echo "shellvar=[$PROBE]"
          echo "stepenv=[$COPIED]"
          echo "declared=[${{ env.DECLARED }}]"
"#;

fn assert_step_env_config_lines(report: &RunReportPlus) {
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(report);
    for expected in [
        // The inline expression, the variable and the step-env copy agree:
        // one substitution at spawn, from the environment the process
        // receives, so the expression and the variable cannot diverge.
        "inline=[from_env_file]",
        "shellvar=[from_env_file]",
        "stepenv=[from_env_file]",
        // A name declared in the workflow still answers, through the scope
        // env the process inherits.
        "declared=[scope-value]",
    ] {
        assert!(
            lines.contains(&expected.to_string()),
            "{expected}: {lines:?}"
        );
    }
}

/// Inline `${{ env.NAME }}` in a later step's config sees what an earlier step
/// appended through `GITHUB_ENV` — on GitHub the runner renders step config
/// with that environment — and lands on exactly the value the process
/// receives, in `run:` text and `env:` values alike.
#[tokio::test]
async fn step_config_reads_the_step_environment() {
    let graph = lower_ok(STEP_ENV_CONFIG_WORKFLOW);
    let report = run_host(graph, "env-config").await;
    assert_step_env_config_lines(&report);
}

/// The same truth holds when the job runs in a container: the substitution
/// happens where the environment is known, so the mount point changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn step_config_reads_the_step_environment_in_a_container() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let text = STEP_ENV_CONFIG_WORKFLOW.replace(
        "    runs-on: ubuntu-latest\n",
        &format!("    runs-on: ubuntu-latest\n    container: {RUNNER_IMAGE_2404}\n"),
    );
    let graph = lower_ok(&text);
    let report = run_host(graph, "env-config-boxed").await;
    assert_step_env_config_lines(&report);
}

/// The documented pattern for secrets and conditions: pass the secret through
/// an environment variable and test it in the step. The gate resolves the
/// secret at spawn, step-side, so nothing of it reaches the log.
#[tokio::test]
async fn a_secret_passed_through_env_gates_a_step() {
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: gated
        if: env.HAS_TOKEN != ''
        env:
          HAS_TOKEN: ${{ secrets.DEPLOY_TOKEN }}
        run: echo deploying
      - id: absent
        if: env.NOT_SET != ''
        run: echo never
";
    let graph = lower_ok(text);
    let report =
        run_host_with_secrets(graph, "secret-env-if", &[("DEPLOY_TOKEN", "t0ps3cret")]).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    assert!(lines.contains(&"deploying".to_string()), "{lines:?}");
    assert!(!lines.contains(&"never".to_string()), "{lines:?}");
    assert_eq!(status_of(&report, "j/absent").as_deref(), Some("skipped"));
    assert!(
        !lines.iter().any(|l| l.contains("t0ps3cret")),
        "the secret stays out of the log"
    );
}

/// A null expression renders **empty**, as GitHub's coercion table says —
/// never the JSON spelling `null`, which actions then read as a real value.
/// (The corpus sweep caught the setup-* family receiving the string "null"
/// for inputs whose expressions had nothing to say locally: `scandir 'null'`,
/// `Unable to find Go version 'null'`.) Both coercion paths are pinned: a
/// whole-expression env value, and an inline template in the script.
#[tokio::test]
async fn null_expressions_render_empty() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          WHOLE: ${{ github.event.nope }}
        run: echo "whole=[$WHOLE] inline=[${{ github.event.nope }}]"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "null-empty").await;
    assert_eq!(report.status, ir::RunStatus::Success);
    assert!(
        log_lines(&report).iter().any(|l| l == "whole=[] inline=[]"),
        "{:?}",
        log_lines(&report)
    );
}

/// `${{ github.workspace }}` resolves to the runner-side workspace path — the
/// same value as `GITHUB_WORKSPACE` — in every position: a whole-expression
/// env value, an inline script template, a larger template, and a gate. The
/// lowering cannot know the path (a host path here, a mount point in a
/// container), so it rides a sentinel the step substitutes at spawn; the
/// corpus sweep caught the setup-* family receiving nothing at all (setup-uv's
/// `working-directory` input *defaults* to this context).
#[tokio::test]
async fn github_workspace_resolves_to_the_runner_side_path() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          WS: ${{ github.workspace }}
        run: |
          [ "$WS" = "$GITHUB_WORKSPACE" ] && echo env-matches
          [ "${{ github.workspace }}" = "$GITHUB_WORKSPACE" ] && echo inline-matches
          [ "${{ github.workspace }}/sub" = "$GITHUB_WORKSPACE/sub" ] && echo template-matches
      - if: github.workspace != ''
        run: echo gate-saw-a-path
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "workspace-context").await;
    assert_eq!(report.status, ir::RunStatus::Success);
    let lines = log_lines(&report);
    for expected in [
        "env-matches",
        "inline-matches",
        "template-matches",
        "gate-saw-a-path",
    ] {
        assert!(lines.iter().any(|l| l == expected), "{expected}: {lines:?}");
    }
}

/// `${{ runner.temp }}` resolves to the same path `RUNNER_TEMP` carries, in
/// the same positions as `github.workspace` above: an env value, an inline
/// script, a template, and a gate. Before the sentinel, the context rendered
/// empty at lowering while the variable was set at runtime — gh-aw's prompt
/// containment check compares exactly these two and failed on the mismatch.
#[tokio::test]
async fn runner_temp_resolves_to_the_runner_side_path() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          TD: ${{ runner.temp }}
        run: |
          [ "$TD" = "$RUNNER_TEMP" ] && echo env-matches
          [ "${{ runner.temp }}" = "$RUNNER_TEMP" ] && echo inline-matches
          [ "${{ runner.temp }}/sub" = "$RUNNER_TEMP/sub" ] && echo template-matches
      - if: runner.temp != ''
        run: echo gate-saw-a-path
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "runner-temp-context").await;
    assert_eq!(report.status, ir::RunStatus::Success);
    let lines = log_lines(&report);
    for expected in [
        "env-matches",
        "inline-matches",
        "template-matches",
        "gate-saw-a-path",
    ] {
        assert!(lines.iter().any(|l| l == expected), "{expected}: {lines:?}");
    }
}

/// `${{ runner.tool_cache }}` resolves to the same path `RUNNER_TOOL_CACHE`
/// carries, in the same four positions as `runner.temp` above. Before the
/// sentinel, the context rendered empty at lowering while the variable was
/// resolved at exec time by the shell prologue; now one computation feeds
/// both, per environment.
#[tokio::test]
async fn runner_tool_cache_resolves_to_the_runner_side_path() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          TC: ${{ runner.tool_cache }}
        run: |
          [ "$TC" = "$RUNNER_TOOL_CACHE" ] && echo env-matches
          [ "${{ runner.tool_cache }}" = "$RUNNER_TOOL_CACHE" ] && echo inline-matches
          [ "${{ runner.tool_cache }}/node" = "$RUNNER_TOOL_CACHE/node" ] && echo template-matches
      - if: runner.tool_cache != ''
        run: echo gate-saw-a-path
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "runner-tool-cache-context").await;
    assert_eq!(report.status, ir::RunStatus::Success);
    let lines = log_lines(&report);
    for expected in [
        "env-matches",
        "inline-matches",
        "template-matches",
        "gate-saw-a-path",
    ] {
        assert!(lines.iter().any(|l| l == expected), "{expected}: {lines:?}");
    }
}

/// Every `run:` step stages its resolved script — which can hold resolved
/// secret plaintext — and scrubs it once the process ends, so a workspace
/// retained after the run keeps no resolved secret on disk. The whole run dir
/// is swept for the plaintext, so a new write-down would fail here by name.
#[tokio::test]
async fn staged_scripts_are_scrubbed_and_no_secret_survives_on_disk() {
    use std::time::Duration;
    use std::{env, fs, process};

    use runtime::executor::{MapSecrets, Retention};
    use runtime::{RunOptions, Runtime};

    const SECRET: &[u8] = b"s3same-scrub-canary";
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo "deploying with ${{ secrets.DEPLOY_KEY }}"
      - run: echo done
        shell: bash -e {0}
"#;
    let graph = with_params(lower_ok(text));
    let dir = env::temp_dir()
        .join("petri-gha")
        .join(format!("script-scrub-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut options = RunOptions::new(&dir);
    options.grace = Duration::from_secs(1);
    options.retention = Retention::Always;
    let report = Runtime::standard()
        .options(options)
        .step(github_actions::RunStep)
        .secrets(MapSecrets::from_pairs(&[
            ("GITHUB_TOKEN", ""),
            ("DEPLOY_KEY", "s3same-scrub-canary"),
        ]))
        .run(graph)
        .await
        .expect("replay is byte-identical");
    assert_eq!(report.status, RunStatus::Success);

    // Walk the retained run dir: the staged scripts exist and are empty, and
    // no file anywhere holds the plaintext.
    let mut scripts = Vec::new();
    let mut leaked = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let bytes = fs::read(&path).unwrap_or_default();
            if path.file_name().is_some_and(|n| n == "script")
                && path.to_string_lossy().contains("/.ci/github/steps/")
            {
                scripts.push((path.clone(), bytes.len()));
            }
            if bytes.windows(SECRET.len()).any(|w| w == SECRET) {
                leaked.push(path);
            }
        }
    }
    assert_eq!(scripts.len(), 2, "both steps staged a script: {scripts:?}");
    assert!(
        scripts.iter().all(|(_, len)| *len == 0),
        "every staged script is scrubbed: {scripts:?}"
    );
    assert!(leaked.is_empty(), "resolved secret found in {leaked:?}");
    let _ = fs::remove_dir_all(&dir);
}
