//! `fabro/wait`: sleep for the node's `duration`, cancellation aware.

use std::time::Duration;

use frontend_attractor::kinds::{StageOutcome, WAIT_KIND};
use ir::{Control, Outcome, StepKindId};
use serde::Deserialize;
use serde_json::json;
use steps::{Step, StepCtx};
use tokio::time;

use crate::outcome::Stage;

pub const KIND: StepKindId = WAIT_KIND;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitConfig {
    pub label:       String,
    pub duration_ms: u64,
}

pub struct WaitStep;

#[async_trait::async_trait]
impl Step for WaitStep {
    const NAME: &'static str = "fabro/wait";
    type Config = WaitConfig;

    async fn run(&self, config: WaitConfig, mut ctx: StepCtx) -> Outcome {
        let sleep = time::sleep(Duration::from_millis(config.duration_ms));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                ctl = ctx.control.recv() => match ctl {
                    Some(Control::Deliver(_)) => {}
                    _ => return Outcome::cancelled(),
                },
            }
        }
        let mut stage = Stage::new(StageOutcome::Succeeded, None);
        stage
            .output
            .insert("waited_ms".into(), json!(config.duration_ms));
        stage.into_outcome(&ctx.node)
    }
}
