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

use execution::hooks::{HookDecision, HookPoint, HookServiceHandle};
use executor::ExecEnv;
use frontend_fabro::hooks::HookEvent;
use frontend_fabro::kinds::{STAGE_KIND, StageOutcome};
use ir::{Outcome, ScopeId, StepKindId, Value};
use serde::Deserialize;
use steps::{Step, StepCtx};

use crate::hooks::{BLOCKED_CLASS, Decision, report_event};
use crate::outcome::Stage;
use crate::{LocalHooksHandle, checkout};

pub const KIND: StepKindId = STAGE_KIND;

/// The run's identity as hooks see it: `FABRO_RUN_ID` and the `run_id` field
/// of every hook context. Set by the per-run provisioner from the run
/// directory's name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunInfo {
    pub run_id: String,
}

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

    /// The first environment recorded for a scope stays: a branch child runs
    /// in the parent's sandbox under its own execution-local scope id, and its
    /// handle is released when the child ends, so it must not replace the
    /// run's own. [`remove`](Self::remove) clears a scope when it is released.
    pub fn record(&self, scope: ScopeId, env: Arc<dyn ExecEnv>) {
        self.envs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(scope)
            .or_insert(env);
    }

    /// Forget a scope's environment: it is being released.
    pub fn remove(&self, scope: ScopeId) {
        self.envs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&scope);
    }

    /// The scope's environment, or, when that scope has not run a step yet,
    /// any recorded environment (every scope of a standalone Fabro run
    /// shares one sandbox).
    pub fn get(&self, scope: ScopeId) -> Option<Arc<dyn ExecEnv>> {
        let envs = self.envs.lock().unwrap_or_else(PoisonError::into_inner);
        envs.get(&scope).or_else(|| envs.values().next()).cloned()
    }

    /// Any recorded environment: where a run-level hook runs.
    pub fn any(&self) -> Option<Arc<dyn ExecEnv>> {
        self.envs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .next()
            .cloned()
    }
}

/// Record the step's environment for hooks that run in its scope.
pub fn record(ctx: &StepCtx) {
    if let Some(local) = ctx.capability::<LocalHooksHandle>() {
        local.0.environments().record(ctx.scope, ctx.env.clone());
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct StageConfig {
    pub node:     String,
    pub kind:     String,
    pub label:    String,
    /// The run's merged `[[run.hooks]]`, on `start` and `exit`.
    pub hooks:    Value,
    /// The workflow's name, for `FABRO_WORKFLOW`.
    pub workflow: String,
    /// The run context at spawn.
    pub kv:       Value,
    /// `[run.clone]` and the repository the host bound, on the root
    /// `start` stage: the workspace is checked out from it first.
    pub checkout: Value,
    #[serde(flatten)]
    pub rest:     serde_json::Map<String, Value>,
}

impl Default for StageConfig {
    fn default() -> Self {
        Self {
            node:     String::new(),
            kind:     String::new(),
            label:    String::new(),
            hooks:    Value::Null,
            workflow: String::new(),
            kv:       Value::Null,
            checkout: Value::Null,
            rest:     serde_json::Map::new(),
        }
    }
}

pub struct StageStep;

/// The run-level hooks a stage fires, in Fabro's order. `start` runs
/// `sandbox_ready` (the sandbox exists once the first step runs in it) then
/// `run_start`; both block. `run_complete`, `run_failed` and
/// `sandbox_cleanup` are not a stage's: the local hook service runs them
/// from the driver's run-end and scope-release points, by the run's final
/// status, with the sandbox still in place.
#[async_trait::async_trait]
impl Step for StageStep {
    const NAME: &'static str = "fabro/stage";
    type Config = StageConfig;

    async fn run(&self, config: StageConfig, mut ctx: StepCtx) -> Outcome {
        record(&ctx);
        // The checkout comes first: the sandbox is "ready" once the
        // repository is in it, as Fabro's clone precedes `sandbox_ready`.
        if config.kind == "start"
            && let Err(error) = checkout::seed(&ctx, &config.checkout).await
        {
            return Stage::failed(
                format!("checkout: {error}"),
                checkout::CLASS,
                Some(frontend_fabro::Policy::Exit),
            )
            .into_outcome(&config.node);
        }
        let Some(handle) = ctx.capability::<HookServiceHandle>() else {
            return Outcome::success(Value::Object(config.rest));
        };
        let Some(local) = ctx.capability::<LocalHooksHandle>() else {
            return Outcome::success(Value::Object(config.rest));
        };
        let local = &local.0;
        if !config.hooks.is_null() {
            local.configure_from(&serde_json::json!({
                "hooks": config.hooks,
                "workflow": config.workflow,
            }));
        }
        let events: &[(HookEvent, HookPoint)] = match config.kind.as_str() {
            "start" => &[
                (HookEvent::SandboxReady, HookPoint::BeforeVisit),
                (HookEvent::RunStart, HookPoint::BeforeVisit),
                (HookEvent::StageStart, HookPoint::BeforeAttempt),
            ],
            _ => &[],
        };
        let _ = &handle;
        for (event, point) in events {
            let (decision, mut report) = if *event == HookEvent::StageStart {
                local.start_stage(&ctx, &config.label).await
            } else {
                let context = local.context(*event);
                local
                    .dispatch(*point, &context, Some(ctx.env.clone()))
                    .await
            };
            if *event == HookEvent::StageStart
                && let Decision::Skip { .. } = &decision
            {
                report.decision = HookDecision::Skip {
                    status: ir::Status::Skipped,
                };
                let _ = ctx
                    .logs
                    .send(report_event(
                        &ctx.node,
                        ctx.firing,
                        ctx.attempt,
                        *event,
                        &report,
                    ))
                    .await;
                return Stage::new(StageOutcome::Skipped, None).into_outcome(&config.node);
            }
            if let Decision::Block { reason } = &decision {
                report.decision = HookDecision::Block {
                    reason: reason
                        .clone()
                        .unwrap_or_else(|| format!("blocked by {} hook", event_title(*event))),
                };
            }
            let silent = report.is_silent();
            if !silent {
                let _ = ctx
                    .logs
                    .send(report_event(
                        &ctx.node,
                        ctx.firing,
                        ctx.attempt,
                        *event,
                        &report,
                    ))
                    .await;
            }
            if let HookDecision::Block { reason } = report.decision {
                let _ = &mut ctx;
                return Stage::failed(
                    format!("blocked: {reason}"),
                    BLOCKED_CLASS,
                    Some(frontend_fabro::Policy::Exit),
                )
                .into_outcome(&config.node);
            }
        }
        let mut stage = Stage::new(StageOutcome::Succeeded, None);
        for (key, value) in config.rest {
            stage.output.insert(key, value);
        }
        stage.into_outcome(&config.node)
    }
}

fn event_title(event: HookEvent) -> &'static str {
    match event {
        HookEvent::RunStart => "RunStart",
        HookEvent::SandboxReady => "SandboxReady",
        HookEvent::RunComplete => "RunComplete",
        other => other.as_str(),
    }
}
