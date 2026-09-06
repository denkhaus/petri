//! The two agent transports share the step's output contract and repair loop.

use std::collections::BTreeMap;
use std::time::Duration;

use ir::{Control, Value};
use pebble_coding_agent::ShutdownReason;
use serde::Deserialize;
use smol_str::SmolStr;
use steps::StepCtx;
use tokio::sync::mpsc;

use super::AgentConfig;
use crate::acp::{AcpError, Client};
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
    pub(crate) async fn open(config: &AgentConfig, ctx: &mut StepCtx) -> Result<Self, AgentError> {
        match config.backend {
            AgentBackend::Api => NativeSession::open(config, ctx)
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
                if let Err(error) = client.open_session(ctx.env.workspace_path()).await {
                    client.terminate(ctx.env.grace()).await;
                    return Err(error.into());
                }
                Ok(Self::Acp(client))
            }
        }
    }
    pub(crate) async fn prompt(
        &mut self,
        text: &str,
        control: &mut mpsc::Receiver<Control>,
        grace: Duration,
    ) -> Result<String, AgentError> {
        match self {
            Self::Acp(client) => client
                .prompt(text, control, grace)
                .await
                .map(|turn| turn.text)
                .map_err(Into::into),
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
    pub(crate) fn metrics(&self, turns: u64) -> BTreeMap<SmolStr, Value> {
        match self {
            Self::Acp(_) => BTreeMap::from([("acp.turns".into(), Value::from(turns))]),
            Self::Pebble(session) => session.metrics(),
        }
    }
}
