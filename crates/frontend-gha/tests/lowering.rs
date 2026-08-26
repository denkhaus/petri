//! Handoff §7 tests 3–6: status truth table, matrix, composites, secrets — through
//! the engine wherever the claim is about behaviour rather than shape.

mod support;

use frontend::Severity;
use ir::RunStatus;
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
    assert!(printed.contains("matrix_combinations"), "{printed}");
    assert!(printed.contains("from_json"), "{printed}");

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

#[test]
fn composite_depth_cap_is_a_diagnostic() {
    let recursive = r#"
runs:
  using: composite
  steps:
    - uses: ./.github/actions/loop
"#;
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: ./.github/actions/loop
"#;
    let files = files(&[(".github/actions/loop/action.yml", recursive)]);
    let diags = diagnostics_with(text, &files);
    let depth = diags
        .iter()
        .find(|d| d.code == "gha.composite_depth")
        .expect("depth cap reported");
    assert_eq!(depth.severity, Severity::Error);
}

// ── §7 test 6: secrets ────────────────────────────────────────────────────

#[test]
fn secrets_lower_to_refs_in_env_and_are_errors_elsewhere() {
    let text = r#"
on: push
env:
  TOP: ${{ secrets.TOP_TOKEN }}
jobs:
  j:
    runs-on: ubuntu-latest
    env:
      JOB: ${{ secrets.JOB_TOKEN }}
    steps:
      - env:
          STEP: ${{ secrets.STEP_TOKEN }}
        run: echo "$TOP $JOB $STEP"
"#;
    let graph = lower_ok(text);
    let step = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    let env = &step.step.config["env"];
    assert_eq!(
        env["TOP"],
        json!({"$secret": "TOP_TOKEN"}),
        "workflow-level secrets push down into the step"
    );
    assert_eq!(env["JOB"], json!({"$secret": "JOB_TOKEN"}));
    assert_eq!(env["STEP"], json!({"$secret": "STEP_TOKEN"}));
    // No secret value, and no secret *reference* in scope env either.
    let encoded = serde_json::to_string(&graph).unwrap();
    assert!(!encoded.contains("s3cret"));
    for scope in &graph.scopes {
        assert!(
            !scope.env.contains_key("TOP"),
            "secret refs do not live in scope env"
        );
    }

    for bad in ["    if: ${{ secrets.X == 'y' }}", "    if: secrets.X"] {
        let text = format!(
            r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
{bad}
    steps:
      - run: echo hi
"#
        );
        let diags = diagnostics(&text);
        assert!(
            diags
                .iter()
                .any(|d| d.code == "unsupported.secrets.expression"),
            "{bad}: {diags:?}"
        );
    }
    // Inside a larger string, even in env position.
    let diags = diagnostics(
        r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          URL: https://user:${{ secrets.PW }}@host
        run: echo hi
"#,
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == "unsupported.secrets.expression"),
        "{diags:?}"
    );
}

// ── The rejection set ─────────────────────────────────────────────────────

#[test]
fn the_rejection_set_is_loud_and_specific() {
    let cases: &[(&str, &str)] = &[
        (
            "concurrency: group-a\non: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n",
            "unsupported.concurrency",
        ),
        (
            "on: { workflow_call: {} }\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n",
            "unsupported.workflow_call",
        ),
        (
            "on: { workflow_dispatch: { inputs: { x: { type: string } } } }\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n",
            "unsupported.workflow_dispatch.inputs",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    services: { db: { image: postgres } }\n    steps:\n      - run: echo\n",
            "unsupported.services",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: windows-latest\n    steps:\n      - run: echo\n",
            "unsupported.runs_on.windows",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: [self-hosted, gpu]\n    steps:\n      - run: echo\n",
            "unsupported.runs_on.unknown",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: ${{ matrix.os }}\n    strategy: { matrix: { os: [a] } }\n    steps:\n      - run: echo\n",
            "unsupported.runs_on.expression",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
            "unsupported.action.remote",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        shell: pwsh\n",
            "unsupported.shell.pwsh",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ hashFiles('**/lock') }}\n",
            "unsupported.expression.hashFiles",
        ),
        (
            "on: push\njobs:\n  j:\n    uses: org/repo/.github/workflows/x.yml@main\n",
            "unsupported.workflow_call",
        ),
    ];
    for (text, code) in cases {
        let diags = diagnostics(text);
        let found = diags.iter().find(|d| d.code == *code);
        assert!(
            found.is_some(),
            "expected {code} for:\n{text}\ngot {diags:#?}"
        );
        let d = found.unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.hint.is_some(), "{code} needs a hint");
        assert!(d.span.line > 0, "{code} needs a span");
    }
    // The remote-action message names the action, for the histogram.
    let diags = diagnostics(
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
    );
    let d = diags
        .iter()
        .find(|d| d.code == "unsupported.action.remote")
        .unwrap();
    assert!(d.message.contains("actions/checkout@v4"), "{}", d.message);
}

#[test]
fn a_clean_workflow_lowers_to_the_documented_shape() {
    let text = r#"
name: ci
on: push
env:
  GLOBAL: g
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      LOCAL: l
    outputs:
      artifact: ${{ steps.compile.outputs.artifact }}
    steps:
      - id: compile
        run: echo "artifact=app.tar" >> "$GITHUB_OUTPUT"
        timeout-minutes: 5
  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo "testing ${{ needs.build.outputs.artifact }}"
  lint:
    needs: build
    runs-on: ubuntu-latest
    container: alpine:3.20
    steps:
      - run: echo lint
"#;
    let graph = lower_ok(text);
    let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "build/start",
            "build/done",
            "test/start",
            "test/done",
            "lint/start",
            "lint/done",
            "build/compile",
            "test/step-1",
            "lint/step-1"
        ]
    );
    // k dependents → k single-arm groups on `done`.
    let build_done = graph.nodes.iter().find(|n| n.name == "build/done").unwrap();
    assert_eq!(build_done.routing.groups.len(), 2);
    assert!(build_done.routing.groups.iter().all(|g| g.arms.len() == 1));
    // needs → All join on the dependent's start.
    let test_start = graph.nodes.iter().find(|n| n.name == "test/start").unwrap();
    assert_eq!(test_start.join, ir::JoinPolicy::All);
    // timeout-minutes → budget.
    let compile = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/compile")
        .unwrap();
    assert_eq!(compile.budget.timeout.as_secs(), 300);
    // container → Docker scope; runs-on → requirements.
    let lint_scope = graph
        .scope(
            graph
                .nodes
                .iter()
                .find(|n| n.name == "lint/step-1")
                .unwrap()
                .scope,
        )
        .unwrap();
    assert!(
        matches!(&lint_scope.runtime.target, ir::RuntimeTarget::Docker { image, .. } if image == "alpine:3.20")
    );
    assert_eq!(
        lint_scope.runtime.requirements,
        vec![smol_str::SmolStr::new("ubuntu-latest")]
    );
    // Env layering.
    let build_scope = graph.scope(compile.scope).unwrap();
    assert!(build_scope.env.contains_key("GLOBAL"));
    assert!(build_scope.env.contains_key("LOCAL"));
    assert!(build_scope.env.contains_key("GITHUB_SHA"));
    // GHA exercises the degenerate subset.
    assert!(!graph.edges().any(|e| e.back));
    assert!(graph.nodes.iter().all(|n| n.join == ir::JoinPolicy::All));
    ir::validate(&graph).expect("valid");
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
