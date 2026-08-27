//! Handoff §7 tests 3–6, the pure half: what lowering produces, without running it.
//! The behavioural half — the same constructs through the engine on real processes —
//! lives in `crates/github/acceptance/tests/gha_e2e.rs`.

mod support;

use frontend::Severity;
use serde_json::json;
use support::*;

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

#[test]
fn custom_shells_lower_to_a_template_and_windows_shells_stay_rejected() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo login
        shell: bash -leo pipefail {0}
      - run: print("hi")
        shell: python
      - run: Get-Location
        shell: pwsh
      - run: echo env
        shell: /usr/bin/env bash {0}
"#;
    let graph = lower_ok(text);
    let shell_command = |name: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .step
            .config["shell_command"]
            .clone()
    };
    assert_eq!(shell_command("j/step-1"), json!("bash -leo pipefail {0}"));
    assert_eq!(shell_command("j/step-2"), json!("python {0}"));
    assert_eq!(shell_command("j/step-3"), json!("pwsh -command \". '{0}'\""));
    assert_eq!(shell_command("j/step-4"), json!("/usr/bin/env bash {0}"));

    let diags = diagnostics(
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        shell: cmd\n",
    );
    assert!(
        diags.iter().any(|d| d.code == "unsupported.shell.cmd"),
        "{diags:?}"
    );
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
    // Inside a larger string in step config it lowers: the step resolves the
    // secret's sentinel at spawn, and the log never sees the value.
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
        !diags
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
            // `pwsh`, `python` and `{0}` templates lower now; Windows shells stay.
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        shell: cmd\n",
            "unsupported.shell.cmd",
        ),
        (
            // In `run:` a literal-pattern hashFiles lowers (the step resolves it);
            // in a position the engine evaluates it stays rejected.
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        if: hashFiles('**/lock') != ''\n",
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
        matches!(&lint_scope.runtime.target, ir::RuntimeTarget::Container { image } if image == "alpine:3.20")
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

// ── Cancellation flags ────────────────────────────────────────────────────

/// Where `run_on_cancel` lands (spec §5): on steps gated `always()` or
/// `cancelled()`, on every node of a job so gated, on every inlined node of a
/// composite so gated at the caller, and on every `done` unconditionally.
/// Nothing else carries it.
#[test]
fn run_on_cancel_lands_exactly_where_github_keeps_going() {
    let action = r#"
runs:
  using: composite
  steps:
    - shell: bash
      run: echo one
    - shell: bash
      run: echo two
"#;
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo build
      - id: tidy
        if: always()
        run: echo tidy
      - id: onfail
        if: failure()
        run: echo onfail
      - id: sweep
        if: cancelled()
        uses: ./.github/actions/sweeper
      - id: plain
        uses: ./.github/actions/sweeper
  cleanup:
    needs: build
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo cleanup
"#;
    let files = files(&[(".github/actions/sweeper/action.yml", action)]);
    let graph = lower_ok_with(text, &files);

    let flagged: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.run_on_cancel)
        .map(|n| n.name.as_str())
        .collect();
    let mut expected = vec![
        // done: always, both jobs.
        "build/done",
        "cleanup/done",
        // step-level always()/cancelled(), composites inlined per node.
        "build/tidy",
        "build/sweep/step-1",
        "build/sweep/step-2",
        // job-level always(): every node of the job.
        "cleanup/start",
        "cleanup/step-1",
    ];
    let mut flagged_sorted = flagged.clone();
    flagged_sorted.sort_unstable();
    expected.sort_unstable();
    assert_eq!(flagged_sorted, expected, "flags: {flagged:?}");
}

/// `cancelled()` ORs in the engine's `scope_cancelled` static, at step and job
/// level, so a cancel no step record can show — between steps, before a job
/// starts, a `fail_fast` scope cancel — still reads as cancelled.
#[test]
fn cancelled_lowers_with_scope_cancelled() {
    let text = r#"
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: echo a
  b:
    needs: a
    if: cancelled()
    runs-on: ubuntu-latest
    steps:
      - if: cancelled()
        run: echo b
"#;
    let graph = lower_ok(text);
    let printed = frontend::print_graph(&graph);
    assert!(
        printed.contains("scope_cancelled"),
        "the static appears in the lowered expressions: {printed}"
    );
}
