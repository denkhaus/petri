//! The two agent transports share the step's output contract and repair loop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use execution::hooks::HookServiceHandle;
use frontend_fabro::hooks::HookEvent;
use ir::{Control, Value};
use pebble_coding_agent::{CodingAgentExport, ShutdownReason};
use serde::Deserialize;
use smol_str::SmolStr;
use steps::StepCtx;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::AgentConfig;
use crate::LocalHooksHandle;
use crate::acp::{AcpError, AcpHooks, Client};
use crate::pebble::NativeSession;

/// How an agent node runs. ACP remains the default.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    #[default]
    Acp,
    Api,
}

pub(crate) enum Session {
    Acp(Client),
    Pebble(Box<NativeSession>),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    #[error("agent cancelled")]
    Cancelled,
    #[error("{message}")]
    Failed { class: String, message: String },
}
impl AgentError {
    pub(crate) fn failed(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failed {
            class:   class.into(),
            message: message.into(),
        }
    }
}
impl From<AcpError> for AgentError {
    fn from(error: AcpError) -> Self {
        match error {
            AcpError::Cancelled => Self::Cancelled,
            AcpError::StopReason(reason) => Self::failed(
                format!("stop_reason:{reason}"),
                format!("the agent stopped with `{reason}`"),
            ),
            other => Self::failed("acp_protocol", other.to_string()),
        }
    }
}

impl Session {
    /// Open the node's session. `resume` is a retained thread's conversation
    /// a native node continues; an ACP node ignores it (ACP never reuses
    /// threads, and the caller has said so).
    pub(crate) async fn open(
        config: &AgentConfig,
        ctx: &mut StepCtx,
        resume: Option<CodingAgentExport>,
    ) -> Result<Self, AgentError> {
        match config.backend {
            AgentBackend::Api => NativeSession::open(config, ctx, resume)
                .await
                .map(|session| Self::Pebble(Box::new(session))),
            AgentBackend::Acp => {
                let command = config
                    .command()
                    .map_err(|e| AgentError::failed("acp_unconfigured", e))?;
                if config.model.is_some()
                    || config.provider.is_some()
                    || config.reasoning_effort.is_some()
                {
                    tracing::warn!(node = %config.node, "the ACP command owns model selection; model, provider and reasoning_effort are observer metadata");
                }
                let mut client = Client::spawn(ctx.env.as_ref(), &command, ctx.logs.clone())
                    .await
                    .map_err(|e| AgentError::failed("spawn_failed", e.to_string()))?;
                if let Some(handle) = ctx.capability::<HookServiceHandle>()
                    && let Some(local) = ctx.capability::<LocalHooksHandle>()
                {
                    let names = |event: HookEvent| {
                        local
                            .0
                            .hooks_for(event)
                            .into_iter()
                            .map(|hook| hook.name)
                            .collect::<Vec<_>>()
                    };
                    let mut post = names(HookEvent::PostToolUse);
                    post.extend(names(HookEvent::PostToolUseFailure));
                    let hooks = AcpHooks::new(
                        handle.0.clone(),
                        crate::hooks::step_view(ctx, "agent", &config.label, &config.kv),
                        ctx.node.clone(),
                        ctx.firing,
                        ctx.attempt,
                        names(HookEvent::PreToolUse),
                        post,
                    );
                    if hooks.has_tool_hooks() {
                        client.with_hooks(Arc::new(hooks)).await;
                    }
                }
                if let Err(error) = client.open_session(ctx.env.workspace_path()).await {
                    client.terminate(ctx.env.grace()).await;
                    return Err(error.into());
                }
                Ok(Self::Acp(client))
            }
        }
    }
    /// One prompt turn. `deadline` is the node's `timeout`, which an ACP
    /// agent consumes itself (`TimeoutPolicy::HandlerManaged`, as Fabro hands
    /// its `timeout_ms` to the ACP turn): a turn that outlives it is
    /// terminated and fails with class `timeout`. A native Pebble session
    /// ignores it; the driver's interview-aware timer owns that deadline.
    pub(crate) async fn prompt(
        &mut self,
        text: &str,
        control: &mut mpsc::Receiver<Control>,
        grace: Duration,
        deadline: Option<Duration>,
    ) -> Result<String, AgentError> {
        match self {
            Self::Acp(client) => {
                let turn = client.prompt(text, control, grace);
                let result = match deadline {
                    Some(deadline) => timeout(deadline, turn).await.ok(),
                    None => Some(turn.await),
                };
                let Some(result) = result else {
                    client.terminate(grace).await;
                    return Err(AgentError::failed(
                        "timeout",
                        format!(
                            "the agent turn timed out after {}ms",
                            deadline
                                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                        ),
                    ));
                };
                result.map(|turn| turn.text).map_err(Into::into)
            }
            Self::Pebble(session) => session.prompt(text, control).await,
        }
    }
    pub(crate) async fn shutdown(
        &mut self,
        reason: ShutdownReason,
        grace: Duration,
    ) -> Result<(), AgentError> {
        match self {
            Self::Acp(client) => {
                client.terminate(grace).await;
                Ok(())
            }
            Self::Pebble(session) => session.shutdown(reason).await,
        }
    }
    /// The native conversation, warm, for the next node on its thread. An
    /// ACP session has none.
    pub(crate) fn export(&self) -> Option<CodingAgentExport> {
        match self {
            Self::Acp(_) => None,
            Self::Pebble(session) => Some(session.export()),
        }
    }
    pub(crate) fn metrics(&self, turns: u64) -> BTreeMap<SmolStr, Value> {
        match self {
            Self::Acp(_) => BTreeMap::from([("acp.turns".into(), Value::from(turns))]),
            Self::Pebble(session) => session.metrics(),
        }
    }
}
