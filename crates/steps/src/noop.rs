//! The noop step: runs nothing, and returns its own (resolved) config as its output.
//!
//! Frontends need structural nodes — a job's gate, a matrix collector, the head of a
//! desugared loop — that exist to join, to route, or to compute a summary expression.
//! Because config placeholders are resolved against the firing's environment before
//! the step runs, a noop's config *is* a computed value: `{"result": {"$expr": id}}`
//! comes out the other side with the expression evaluated.
//!
//! It is a step kind, not an engine feature. The core does not know it exists.

use ir::{Outcome, StepKindId};

use crate::ctx::{StepCtx, StepRunner};

/// The step kind id the noop registers under.
pub const NOOP_KIND: StepKindId = StepKindId::new_static("noop");

pub struct NoopStep;

#[async_trait::async_trait]
impl StepRunner for NoopStep {
    fn kind(&self) -> StepKindId {
        NOOP_KIND
    }

    fn name(&self) -> &str {
        "noop"
    }

    async fn run(&self, ctx: StepCtx) -> Outcome {
        Outcome::success(ctx.config)
    }
}
