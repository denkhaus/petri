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

use crate::LocalHooksHandle;
use crate::hooks::{BLOCKED_CLASS, report_event};
use crate::outcome::Stage;

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
        envs.get(&scope).or_else(|| envs.values().next()).cloned()
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
            rest:     serde_json::Map::new(),
        }
    }
}

pub struct StageStep;

/// The run-level hooks a stage fires, in Fabro's order. `start` runs
/// `sandbox_ready` (the sandbox exists once the first step runs in it) then
/// `run_start`; both block. `exit` runs `run_complete`; a run that never
/// reaches `exit` failed, and `fabro/agent` and friends do not know that, so
/// `run_failed` is driven by the host's observer ([`crate::hooks::RunEnd`]).
#[async_trait::async_trait]
impl Step for StageStep {
    const NAME: &'static str = "fabro/stage";
    type Config = StageConfig;

    async fn run(&self, config: StageConfig, mut ctx: StepCtx) -> Outcome {
        record(&ctx);
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
            "exit" => &[(HookEvent::RunComplete, HookPoint::AfterVisit)],
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
                && let crate::hooks::Decision::Skip { .. } = &decision
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
            if let crate::hooks::Decision::Block { reason } = &decision {
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
