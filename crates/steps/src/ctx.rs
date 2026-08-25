//! What a step kind is handed, and what it returns.

use std::sync::Arc;

use executor::{ExecEnv, SecretProvider};
use ir::{Attempt, Control, FiringId, StepEvent, Value};
use smol_str::SmolStr;
use tokio::sync::mpsc;

/// Everything a step needs to run one attempt.
pub struct StepCtx {
    pub firing: FiringId,
    pub attempt: Attempt,
    /// The node's instance name, for log file naming and messages.
    pub node: SmolStr,
    /// The resolved config. Free of expression placeholders; may hold `$secret`
    /// references, which are resolved here at spawn time and never written down.
    pub config: Value,
    pub env: Arc<dyn ExecEnv>,
    pub secrets: Arc<dyn SecretProvider>,
    /// Progress out: logs and artifacts, in arrival order.
    pub logs: mpsc::Sender<StepEvent>,
    /// Control in. A `Cancel` starts the ladder.
    pub control: mpsc::Receiver<Control>,
}

impl StepCtx {
    /// Emit a log line as if the step had printed it.
    pub async fn log(&self, stream: ir::LogStream, line: impl Into<String>) {
        let _ = self
            .logs
            .send(StepEvent::Log {
                stream,
                line: line.into(),
            })
            .await;
    }
}

/// A step kind, as the driver runs it.
///
/// One attempt per call. Retries are the core's business: a runner never loops.
#[async_trait::async_trait]
pub trait StepRunner: Send + Sync {
    fn kind(&self) -> ir::StepKindId;

    fn name(&self) -> &str;

    async fn run(&self, ctx: StepCtx) -> ir::Outcome;
}

/// The step kinds a driver can dispatch to.
#[derive(Default)]
pub struct RunnerRegistry {
    runners: std::collections::HashMap<ir::StepKindId, Arc<dyn StepRunner>>,
}

impl RunnerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, runner: Arc<dyn StepRunner>) -> ir::StepKindId {
        let id = runner.kind();
        self.runners.insert(id, runner);
        id
    }

    pub fn get(&self, id: ir::StepKindId) -> Option<Arc<dyn StepRunner>> {
        self.runners.get(&id).cloned()
    }
}
