//! What a finished execution leaves behind: the invocation result projected
//! from its final state, and the middleware fold rebuilt from its log.

use std::collections::{BTreeMap, BTreeSet};

use ir::{FailureClass, FailureInfo, Graph, ResultProjection, RunStatus, Value};

use crate::middleware::derive_fold_event;
use crate::{ExecutionId, InvocationResult, MiddlewarePipeline};

pub(super) fn project_result(
    execution: ExecutionId,
    mut status: RunStatus,
    graph: &Graph,
    state: &engine::EngineState,
) -> InvocationResult {
    let mut failure = state
        .history()
        .iter()
        .rev()
        .find_map(|record| record.outcome.status.failure_info().cloned());
    if failure.is_none() && status == RunStatus::Failed {
        failure =
            Some(FailureInfo::new(state.errors().last().map_or_else(
                || "execution failed".to_string(),
                ToString::to_string,
            )));
    }
    let mut updates = BTreeMap::new();
    let output = match graph.result {
        ResultProjection::None => Value::Null,
        ResultProjection::NodeOutput(node) => graph
            .node(node)
            .and_then(|node| state.run_context().node(&node.name))
            .map_or_else(
                || {
                    if status == RunStatus::Success {
                        status = RunStatus::Failed;
                        failure = Some(
                            FailureInfo::new("the declared invocation result node did not run")
                                .with_class(FailureClass::new_static("invalid_invocation_result")),
                        );
                    }
                    Value::Null
                },
                |record| {
                    // The result node's own writes, from its final record:
                    // the node record keeps the output, the history the
                    // outcome.
                    if let Some(final_record) = state
                        .history()
                        .iter()
                        .rev()
                        .find(|final_record| final_record.node == node)
                    {
                        updates = final_record.outcome.context_updates.clone();
                    }
                    record.output.clone()
                },
            ),
    };
    InvocationResult {
        status,
        failure,
        final_execution: execution,
        output,
        context: (*state.run_context().kv).clone(),
        updates,
    }
}

pub(super) fn rebuild_middleware(
    pipeline: &MiddlewarePipeline,
    graph: &Graph,
    log: &engine::EventLog,
) -> Result<(), crate::MiddlewareError> {
    let state = engine::replay(graph.clone(), log);
    let final_attempts: BTreeSet<_> = state
        .history()
        .iter()
        .map(|record| (record.firing, record.attempt))
        .collect();
    let nodes_by_firing: BTreeMap<_, _> = state
        .history()
        .iter()
        .map(|record| (record.firing, record.node))
        .collect();
    // Fold the durable prefix here. `Driver::resume` presents any regenerated
    // core suffix to the fold observer before it dispatches pending commands,
    // so folding that suffix here too would count it twice. The derivation is
    // the live observer's own, so the two paths cannot drift.
    for record in log.records() {
        let fold = derive_fold_event(
            &record.event,
            |firing, attempt| final_attempts.contains(&(firing, attempt)),
            |firing| nodes_by_firing.get(&firing).copied(),
            |edge| {
                graph
                    .edge(edge)
                    .is_some_and(|edge| edge.transition == ir::EdgeTransition::Restart)
            },
        );
        if let Some(event) = fold {
            pipeline.fold(&event)?;
        }
    }
    Ok(())
}
