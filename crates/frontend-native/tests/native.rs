//! Handoff §7 test 7: the native format reaches the whole engine, and the
//! invariant-8 error reads as a hint rather than an accident.

use frontend::Severity;
use frontend_native::load;
use ir::{JoinPolicy, RunStatus};

fn lower_ok(text: &str) -> ir::Graph {
    let lowered = load("test.yml", text);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.unwrap_or_else(|| panic!("expected a graph"))
}

fn diagnostics(text: &str) -> Vec<frontend::Diagnostic> {
    load("test.yml", text).diagnostics.into_vec()
}

const CYCLE_XOR_ANY: &str = r#"
name: poll-until-ready
nodes:
  start:
    run: echo "attempt=0" > "$CI_OUTPUT"
    next: poll
  poll:
    join: any
    budget: { max_firings: 5 }
    run: |
      n=$(( ${ATTEMPT:-0} + 1 ))
      echo "attempt=$n" > "$CI_OUTPUT"
      if [ "$n" -ge 3 ]; then echo "ready=true" >> "$CI_OUTPUT"; else echo "ready=false" >> "$CI_OUTPUT"; fi
    config:
      env:
        ATTEMPT: ${{ input.attempt }}
    select:
      - when: ${{ output.ready != 'true' }}
        to: poll
        back: true
      - to: done
  done:
    run: echo ready after ${{ input.attempt }} attempts
"#;

#[test]
fn a_cycle_with_xor_routing_and_an_any_join_lowers() {
    let graph = lower_ok(CYCLE_XOR_ANY);
    let poll = graph.nodes.iter().find(|n| n.name == "poll").unwrap();
    assert_eq!(poll.join, JoinPolicy::Any);
    assert_eq!(poll.routing.groups.len(), 1, "one group: XOR");
    assert_eq!(poll.routing.groups[0].arms.len(), 2);
    assert!(
        poll.routing.groups[0].arms[0].back,
        "the loop arm is a back edge"
    );
    assert!(!poll.routing.groups[0].arms[1].back);
    assert_eq!(poll.budget.max_firings, 5);
    ir::validate(&graph).expect("validates");
}

/// The same graph runs on the real engine and executor: three generations of `poll`,
/// then `done`.
#[tokio::test]
async fn the_cycle_runs_end_to_end() {
    use std::sync::Arc;
    let graph = lower_ok(CYCLE_XOR_ANY);
    let dir = std::env::temp_dir().join(format!("petri-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let executor: Arc<dyn executor::Executor> =
        Arc::new(executor::HostExecutor::new(&dir).with_retention(executor::Retention::Never));
    let mut runners = steps::RunnerRegistry::new();
    runners.register(Arc::new(steps::ProcessStep));
    runners.register(Arc::new(steps::NoopStep));
    let driver = driver::Driver::new(
        graph.clone(),
        executor,
        runners,
        Arc::new(executor::MapSecrets::empty()),
        driver::RunConfig::new(&dir),
    );
    let report = driver.run().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let polls: Vec<u32> = report
        .state
        .history()
        .iter()
        .filter(|r| r.name == "poll")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(polls, vec![0, 1, 2], "three generations, then the exit arm");
    assert_eq!(
        report
            .state
            .history()
            .iter()
            .filter(|r| r.name == "done")
            .count(),
        1
    );
    engine::verify_replay(graph, &report.state.log).expect("replay is byte-identical");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Invariant 8, surfaced with the corollary as its hint.
#[test]
fn an_all_loop_head_gets_the_hinted_diagnostic() {
    let diags = diagnostics(
        r#"
nodes:
  start:
    run: echo go
    parallel: [a, b]
  a:
    run: echo a
    next: head
  b:
    run: echo b
    next: head
  head:
    join: all
    budget: { max_firings: 3 }
    run: echo loop
    select:
      - when: ${{ output.again }}
        to: head
        back: true
      - to: done
  done:
    run: echo done
"#,
    );
    let error = diags
        .iter()
        .find(|d| d.code == "validate.loop_head_must_join_any")
        .expect("invariant 8 is reported");
    assert_eq!(error.severity, Severity::Error);
    assert_eq!(error.span.line, 13, "points at `head`");
    let hint = error.hint.as_deref().expect("has the corollary as a hint");
    assert!(
        hint.contains("join node in front of the loop head"),
        "{hint}"
    );
}

/// `quorum: 1` on a loop head is normalized, not rejected.
#[test]
fn quorum_one_on_a_loop_head_is_normalized_to_any() {
    let graph = lower_ok(
        r#"
nodes:
  start:
    run: echo go
    next: head
  head:
    join: { quorum: 1 }
    budget: { max_firings: 3 }
    run: echo loop
    select:
      - when: ${{ output.again }}
        to: head
        back: true
      - to: done
  done:
    run: echo done
"#,
    );
    let head = graph.nodes.iter().find(|n| n.name == "head").unwrap();
    assert_eq!(head.join, JoinPolicy::Any);
}

/// Fan-out takes `parallel:`; `next:` is always one group.
#[test]
fn fan_out_is_explicit() {
    let graph = lower_ok(
        r#"
nodes:
  start:
    run: echo go
    parallel:
      - lint
      - { to: docs, when: "${{ input.docs }}" }
      - - { to: unit, when: "${{ input.fast }}" }
        - { to: integration }
  lint: { run: echo lint }
  docs: { run: echo docs }
  unit: { run: echo unit }
  integration: { run: echo integration }
"#,
    );
    let start = graph.nodes.iter().find(|n| n.name == "start").unwrap();
    assert_eq!(start.routing.groups.len(), 3, "three groups");
    assert_eq!(
        start.routing.groups[2].arms.len(),
        2,
        "the third is a guarded select"
    );
}

#[test]
fn sequential_for_each_desugars_to_the_documented_cycle() {
    let graph = lower_ok(
        r#"
nodes:
  plan:
    run: echo plan
    next: deploy
  deploy:
    for_each:
      items: ${{ split(output.regions, ',') }}
      parallel: false
      max_iterations: 10
    run: echo ${{ item }}
    next: report
  report:
    run: echo done
"#,
    );
    let plan = graph.nodes.iter().find(|n| n.name == "plan").unwrap();
    let deploy = graph.nodes.iter().find(|n| n.name == "deploy").unwrap();
    assert!(
        plan.routing.groups[0].arms[0].map.is_some(),
        "the entry edge carries the loop state"
    );
    assert_eq!(deploy.join, JoinPolicy::Any, "the head joins with any");
    assert_eq!(
        deploy.routing.groups[0].arms.len(),
        2,
        "back arm + exit arm"
    );
    assert!(deploy.routing.groups[0].arms[0].back);
    assert_eq!(deploy.budget.max_firings, 10);
    ir::validate(&graph).expect("validates");
    // `item` in the body became a lookup on the loop state.
    let printed = frontend::print_graph(&graph);
    assert!(printed.contains("input.items[input.idx]"), "{printed}");
}

#[test]
fn parallel_for_each_becomes_an_expansion() {
    let graph = lower_ok(
        r#"
nodes:
  plan:
    run: echo plan
    next: deploy
  deploy:
    for_each:
      items: ${{ split(input.regions, ',') }}
      max_parallel: 2
      fail_fast: true
    run: echo ${{ item }}
    next: report
  report:
    join: all
    run: echo done
"#,
    );
    let deploy = graph.nodes.iter().find(|n| n.name == "deploy").unwrap();
    match &deploy.expand {
        Some(ir::Expansion::ForEach {
            max_parallel,
            fail_fast,
            target,
            ..
        }) => {
            assert_eq!(*max_parallel, Some(2));
            assert!(fail_fast);
            assert_eq!(*target, ir::ExpandTarget::Node);
        }
        other => panic!("expected an expansion, got {other:?}"),
    }
}

#[test]
fn unknown_bindings_and_keys_are_errors_with_hints() {
    let diags = diagnostics(
        r#"
nodes:
  a:
    run: echo ${{ github.sha }}
    nxt: b
  b:
    run: echo b
"#,
    );
    let binding = diags
        .iter()
        .find(|d| d.code == "expr.unknown_binding")
        .expect("unknown binding");
    assert!(
        binding.hint.as_deref().unwrap_or("").contains("nodes"),
        "{:?}",
        binding.hint
    );
    assert!(diags.iter().any(|d| d.code == "yaml.unknown_key"));
}

#[test]
fn malformed_yaml_is_a_diagnostic_not_a_panic() {
    let diags = diagnostics("nodes: [unclosed");
    assert!(diags.iter().any(|d| d.code == "yaml.syntax"));
    let diags = diagnostics("nodes:\n  a:\n    run: echo ${{ 1 +");
    assert!(
        diags.iter().any(|d| d.code.starts_with("expr.")),
        "{diags:?}"
    );
}
