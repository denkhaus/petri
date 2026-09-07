//! `fabro/stage`: Fabro's structural stages (`start`, `exit`, a
//! conditional) as a step that records where its scope runs.
//!
//! The engine acquires a scope's environment when the first firing in it is
//! admitted, after the admission hooks ran. A hook that must run in the
//! sandbox (`sandbox = true`, the default for command hooks) therefore
//! needs the environment from a step that has already run in that scope.
//! Every Fabro step registers its environment in [`ScopeEnvironments`]
//! through [`record`]; `start` is the first, so `sandbox_ready` and every
//! later stage hook find it. The step itself does what `noop` did: it
//! returns its resolved config as its output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use executor::ExecEnv;
use frontend_fabro::kinds::STAGE_KIND;
use ir::{Outcome, ScopeId, StepKindId, Value};
use steps::{Step, StepCtx};

pub const KIND: StepKindId = STAGE_KIND;

/// The environments of the scopes that have run a Fabro step, by scope id.
/// Registered as a capability by [`crate::register`]; the local hook service
/// runs sandbox-placed hooks through it.
#[derive(Default)]
pub struct ScopeEnvironments {
    envs: Mutex<HashMap<ScopeId, Arc<dyn ExecEnv>>>,
}

impl ScopeEnvironments {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, scope: ScopeId, env: Arc<dyn ExecEnv>) {
        self.envs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(scope, env);
    }

    /// The scope's environment, or, when that scope has not run a step yet,
    /// any recorded environment (every scope of a standalone Fabro run
    /// shares one sandbox).
    pub fn get(&self, scope: ScopeId) -> Option<Arc<dyn ExecEnv>> {
        let envs = self.envs.lock().unwrap_or_else(PoisonError::into_inner);
        envs.get(&scope)
            .or_else(|| envs.values().next())
            .cloned()
    }
}

/// Record the step's environment for hooks that run in its scope.
pub fn record(ctx: &StepCtx) {
    if let Some(envs) = ctx.capability::<ScopeEnvironments>() {
        envs.record(ctx.scope, ctx.env.clone());
    }
}

pub struct StageStep;

#[async_trait::async_trait]
impl Step for StageStep {
    const NAME: &'static str = "fabro/stage";
    type Config = Value;

    async fn run(&self, config: Value, ctx: StepCtx) -> Outcome {
        record(&ctx);
        Outcome::success(config)
    }
}
