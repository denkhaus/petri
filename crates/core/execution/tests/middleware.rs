use std::sync::Arc;

use driver::{DecisionResolver as _, RoutingRequest};
use engine::{
    DecisionId, Intervention, MiddlewareKey, RouteDecision, RoutingCandidate, RoutingProposal,
};
use execution::{
    FoldEvent, Middleware, MiddlewareError, MiddlewarePipeline, RouteCall, RouteNext,
    initial_middleware_state,
};
use ir::{Attempt, EdgeId, EdgeTransition, FiringId, PickPolicy};

struct Override;

#[async_trait::async_trait]
impl Middleware for Override {
    fn key(&self) -> MiddlewareKey {
        MiddlewareKey::new("override")
    }

    fn state_version(&self) -> u32 {
        1
    }

    fn initial_state(&self) -> ir::Value {
        serde_json::json!(0)
    }

    fn fold(&self, state: &mut ir::Value, _event: &FoldEvent<'_>) -> Result<(), MiddlewareError> {
        *state = serde_json::json!(state.as_u64().unwrap_or(0) + 1);
        Ok(())
    }

    async fn route(
        &self,
        _call: RouteCall,
        next: RouteNext,
    ) -> Result<RouteDecision, MiddlewareError> {
        let _ = next.run().await?;
        Ok(RouteDecision::Emit(EdgeId::new(1)))
    }
}

#[tokio::test]
async fn middleware_composes_over_the_core_proposal_and_checkpoints_fold_state() {
    let chain: Vec<Arc<dyn Middleware>> = vec![Arc::new(Override)];
    let pipeline = MiddlewarePipeline::new(
        execution::InvocationId::ROOT,
        execution::ExecutionId::new(0),
        chain.clone(),
        initial_middleware_state(&chain),
    )
    .expect("state versions match");
    let proposal = RoutingProposal {
        group:      0,
        tier:       None,
        pick:       Some(PickPolicy::First),
        candidates: vec![
            RoutingCandidate {
                edge:       EdgeId::new(0),
                weight:     1,
                target:     "first".into(),
                rank:       None,
                transition: EdgeTransition::Continue,
            },
            RoutingCandidate {
                edge:       EdgeId::new(1),
                weight:     1,
                target:     "second".into(),
                rank:       None,
                transition: EdgeTransition::Continue,
            },
        ],
    };
    let resolution = pipeline
        .route(RoutingRequest {
            firing:          FiringId::new(1),
            decision_id:     DecisionId::route(FiringId::new(1), Attempt::FIRST),
            restart_allowed: true,
            groups:          vec![proposal],
        })
        .await
        .expect("middleware resolves");

    assert_eq!(
        resolution.groups[0].decision,
        RouteDecision::Emit(EdgeId::new(1))
    );
    assert!(matches!(
        resolution.groups[0].trace.as_slice(),
        [Intervention::Override { middleware, edge }]
            if middleware.as_str() == "override" && *edge == EdgeId::new(1)
    ));

    pipeline
        .fold(&FoldEvent::ExecutionStarted)
        .expect("fold is pure");
    assert_eq!(
        pipeline.checkpoint()[&MiddlewareKey::new("override")].1,
        serde_json::json!(1)
    );
}
