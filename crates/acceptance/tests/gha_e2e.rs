//! Handoff §7 tests 3–6, the behavioural half: the status truth table, matrices,
//! composites and job outputs, lowered by the real GHA frontend and run on the
//! standard runtime. The pure lowering half stays with the frontend, in
//! `crates/frontend-gha/tests/lowering.rs`.

mod support;

use petri::ir::RunStatus;
use petri::{engine, frontend, ir};
use serde_json::json;
use support::*;

// ── §7 test 3: the status-function truth table ────────────────────────────

/// One job: `a` ends in each status in turn; `b`..`e` are gated on each status
/// function. Which of them ran is the truth table.
fn status_table_workflow(a_step: &str) -> String {
    format!(
        r#"
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
"#
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
        (
            "skipped",
            "        if: false\n        run: exit 0",
            &["b", "d", "f"],
        ),
    ];
    for (label, a_step, expected) in cases {
        let text = status_table_workflow(a_step);
        let graph = lower_ok(&text);
        let report = run_host(graph, &format!("truth-{label}")).await;
        let ran: Vec<String> = ["b", "c", "d", "e", "f"]
            .iter()
            .filter(|s| started(&report).iter().any(|n| n == &format!("j/{s}")))
            .map(|s| s.to_string())
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

/// `cancelled()` is lowered faithfully, and never true today: the engine drops a
/// cancelled firing's tokens, so nothing downstream fires. A spec finding, pinned.
#[tokio::test]
async fn cancelled_steps_cannot_run_today() {
    let text = status_table_workflow("        run: sleep 30");
    let graph = lower_ok(&text);
    let (report, _) = run_host_then_cancel(graph, "truth-cancelled", "j/a").await;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        !started(&report).iter().any(|n| n == "j/e"),
        "the engine does not route after a cancel, so `if: cancelled()` never runs — see the crate docs"
    );
    assert!(
        !started(&report).iter().any(|n| n == "j/d"),
        "nor `if: always()`"
    );
}

/// Job-level status functions read the needed jobs' summaries.
#[tokio::test]
async fn job_level_status_functions_read_needs() {
    let text = r#"
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
"#;
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

/// An expression-valued matrix stays unevaluated in HIR and expands at run time.
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
