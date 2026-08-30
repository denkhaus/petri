//! Handoff §7 tests 1 and 2: real processes, end to end.

mod support;

use ir::{
    Arm, ExpandTarget, GraphBuilder, JoinPolicy, RunStatus, ScopeId, StepRef, collector_exprs,
    parallel_for_each, sequential_for_each, validate,
};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;

/// §7 test 1. The sequential `for_each` example from the design document, run
/// against real bash: three ordered iterations over back edges and generations.
/// Replay of the finished log is byte-identical.
#[tokio::test]
async fn e2e_native_sequential_for_each() {
    let dir = RunDir::new("native-loop");

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    // `plan` emits the items array as its structured output.
    let plan = b.add_node(
        "plan",
        scope,
        StepRef::new(
            PROCESS_KIND,
            script(r#"printf 'regions<<EOF\nus-east\nus-west\neu\nEOF\n' > "$CI_OUTPUT""#),
        ),
    );
    let deploy = add_script(&mut b, "deploy", scope, "echo deploying");
    let report = add_script(&mut b, "report", scope, "echo all regions done");

    // `plan` writes one region per line; the entry edge turns that into the loop
    // state, so the body iterates over a real array.
    let wiring = sequential_for_each(&mut b, plan, deploy, deploy, report, 10);
    b.node_mut(deploy).step = StepRef::new(
        PROCESS_KIND,
        json!({
            "run": "echo \"deploying to $REGION\"; echo \"deployed=$REGION\" > \"$CI_OUTPUT\"",
            "env": { "REGION": { "$expr": wiring.exprs.item.raw() } }
        }),
    );
    let init = {
        let e = b.exprs();
        let regions = e.path("output", &["regions"]);
        let newline = e.lit("\n");
        let items = e.call("split", vec![regions, newline]);
        let zero = e.lit(0);
        let empty = e.array(vec![]);
        e.object(vec![("items", items), ("idx", zero), ("acc", empty)])
    };
    b.node_mut(plan).routing.groups[0].arms[0].map = Some(init);

    let graph = b.build();
    validate(&graph).expect("valid");

    let report_run = host_driver(graph.clone(), &dir).await_run().await;
    assert_eq!(
        report_run.status,
        RunStatus::Success,
        "{:?}",
        report_run.state.errors()
    );

    // Three iterations, in order, each its own generation.
    let generations: Vec<u32> = report_run
        .state
        .history()
        .iter()
        .filter(|r| r.name == "deploy")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(generations, vec![0, 1, 2]);

    let lines = log_lines(&report_run);
    assert!(
        lines.iter().any(|l| l == "deploying to us-east"),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "deploying to us-west"),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l == "deploying to eu"), "{lines:?}");
    assert_eq!(status_of(&report_run, "report").as_deref(), Some("success"));

    assert_replay_identical(&graph, &report_run);
}

/// §7 test 2. The GHA-shaped graph end to end: `needs` fan-out and join, matrix
/// expansion, and `continue-on-error` lowered to `soft_fail` routing on
/// `PartialSuccess`.
#[tokio::test]
async fn e2e_gha_shaped_workflow() {
    let dir = RunDir::new("gha-shaped");

    let mut b = GraphBuilder::bare();
    let build_scope = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let test_scope = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let lint_scope = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let release_scope = b.add_scope(ir::Scope::new(ScopeId::new(0)));

    // job: build
    let checkout = add_script(&mut b, "checkout", build_scope, "echo checked out");
    let compile = b.add_node(
        "compile",
        build_scope,
        StepRef::new(
            PROCESS_KIND,
            script(r#"echo compiling; echo "artifact=app.tar" > "$CI_OUTPUT""#),
        ),
    );
    // job: test, with a matrix
    let setup = add_script(&mut b, "setup", test_scope, "echo setting up");
    let run_tests = add_script(&mut b, "run", test_scope, "echo running suite");
    // job: lint, with continue-on-error
    let lint = b.add_node(
        "lint",
        lint_scope,
        StepRef::new(
            PROCESS_KIND,
            script_with("echo style problems; exit 1", json!({ "soft_fail": [1] })),
        ),
    );
    let publish = add_script(&mut b, "publish", release_scope, "echo publishing");
    let warn = add_script(&mut b, "warn", release_scope, "echo lint was soft-failed");

    let collector = collector_exprs(b.exprs());
    b.link(checkout, compile);
    b.link(setup, run_tests);
    // A job with two dependents: two groups of one arm, explicit fan-out.
    b.fan_out(compile, &[setup, lint]);
    b.select(run_tests, vec![
        Arm::always(publish).with_map(collector.indexed),
    ]);
    // continue-on-error: the soft failure still satisfies the default success
    // guard, and a partial_success guard routes it separately.
    let (succeeded, was_partial) = {
        let e = b.exprs();
        (e.call("success", vec![]), e.call("partial_success", vec![]))
    };
    b.fan_out_groups(lint, vec![vec![Arm::when(publish, succeeded)], vec![
        Arm::when(warn, was_partial),
    ]]);
    // needs: [test, lint]
    b.set_join(publish, JoinPolicy::All);

    let suites = b.exprs().lit(json!(["unit", "integration"]));
    parallel_for_each(
        &mut b,
        setup,
        suites,
        ExpandTarget::Subgraph {
            entry: setup,
            exit:  run_tests,
        },
        None,
        true,
    );

    let graph = b.build();
    validate(&graph).expect("valid");

    let report = host_driver(graph.clone(), &dir).await_run().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    // The matrix ran twice, under its instance names.
    assert!(report.state.run_context().node("setup#0").is_some());
    assert!(report.state.run_context().node("setup#1").is_some());
    assert!(report.state.run_context().node("run#1").is_some());

    // The soft failure is success-like, so `publish` still ran — and the
    // partial_success guard routed `warn` as well.
    assert_eq!(
        status_of(&report, "lint").as_deref(),
        Some("partial_success")
    );
    assert_eq!(status_of(&report, "publish").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "warn").as_deref(), Some("success"));

    // The underlying exit status survives into the record.
    let lint_record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "lint")
        .unwrap();
    assert_eq!(
        lint_record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("exit_status:1")
    );

    // Structured output came back through the outputs file.
    assert_eq!(output_of(&report, "compile")["artifact"], json!("app.tar"));

    assert_replay_identical(&graph, &report);
}
