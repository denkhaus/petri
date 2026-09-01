mod support;

use engine::{EngineExit, EngineStart, EntryPoint, Event};
use ir::{EdgeTransition, GraphBuilder, Outcome, Scope, ScopeId, Value};
use support::Harness;

fn restarting_graph() -> (ir::Graph, ir::NodeId) {
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    let start = builder.add_step("start", scope, "noop");
    let target = builder.add_step("target", scope, "noop");
    builder.mark_entry(start);
    builder.link(start, target);
    builder.node_mut(start).routing.groups[0].arms[0].transition = EdgeTransition::Restart;
    (builder.build(), target)
}

#[test]
fn a_restart_edge_finishes_the_execution_without_emitting_a_token() {
    let (graph, target) = restarting_graph();
    let mut harness = Harness::new(graph);
    harness.feed(Event::ExecutionStarted(EngineStart::default()));
    let starts = harness.take_starts();
    let [(firing, _)] = starts.as_slice() else {
        panic!("the entry starts");
    };
    let firing = *firing;
    harness.finish(firing, Outcome::success(Value::Null));

    assert!(matches!(
        harness.state.exit(),
        Some(EngineExit::Restart { source, target: actual, .. })
            if *source == firing && *actual == target
    ));
    assert_eq!(
        harness
            .state
            .log
            .events()
            .filter(|event| matches!(event, Event::RoutingResolved { .. }))
            .count(),
        1
    );
}

#[test]
fn a_successor_can_force_a_target_that_has_an_ordinary_incoming_edge() {
    let (graph, target) = restarting_graph();
    let mut harness = Harness::new(graph);
    harness.feed(Event::ExecutionStarted(EngineStart {
        entry: EntryPoint::Node(target),
        execution_index: 1,
        ..EngineStart::default()
    }));

    assert_eq!(harness.take_starts()[0].1, "target");
}

#[test]
fn the_core_refuses_a_restart_at_the_execution_limit() {
    let (graph, _) = restarting_graph();
    let mut harness = Harness::new(graph);
    harness.feed(Event::ExecutionStarted(EngineStart {
        execution_index: 1,
        max_executions: 2,
        ..EngineStart::default()
    }));
    let firing = harness.take_starts()[0].0;
    harness.finish(firing, Outcome::success(Value::Null));

    assert!(matches!(
        harness.state.exit(),
        Some(EngineExit::Terminal {
            status: ir::RunStatus::Failed,
        })
    ));
}
