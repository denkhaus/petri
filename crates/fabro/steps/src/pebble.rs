//! Native Pebble sessions. The embedding application owns the model client.

mod capture;
pub mod environment;
pub mod questions;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use environment::{PebbleEnvironment, elapsed_ms};
use questions::AgentQuestions;
use executor::Masker;
use ir::{Attempt, Control, FiringId, ScopeId, StepEvent, Value};
use lithos_llm::Client;
use lithos_llm::types::ReasoningEffort;
use pebble_coding_agent::events::{
    CodingAgentEvent, EventSink, EventSinkError, PermissionLevel, TokenUsage,
};
use pebble_coding_agent::extensions::Redactor;
use pebble_coding_agent::{CodingAgent, CodingAgentOptions, PromptReport, ShutdownReason};
use serde_json::json;
use smol_str::SmolStr;
use steps::StepCtx;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::agent::AgentConfig;
use crate::agent::backend::AgentError;

/// Host capability supplied by applications embedding the native backend.
/// Construct the client with the application's catalog, credentials, and
/// retry policy. The capability owns the client on Petri's host.
#[derive(Clone)]
pub struct PebbleClient(pub Client);

pub(crate) struct NativeSession {
    agent:           CodingAgent,
    questions:       Arc<AgentQuestions>,
    cancel:          CancellationToken,
    kill:            CancellationToken,
    _cancel_on_drop: DropGuard,
    usage:           TokenUsage,
    cost:            Option<u64>,
    inference:       Duration,
    tool:            Duration,
    prompts:         u64,
}

impl NativeSession {
    pub(crate) async fn open(config: &AgentConfig, ctx: &mut StepCtx) -> Result<Self, AgentError> {
        let client = ctx.capability::<PebbleClient>().ok_or_else(|| {
            AgentError::failed(
                "pebble_unconfigured",
                "Native Pebble requires a PebbleClient capability",
            )
        })?;
        if config.acp.is_some() {
            return Err(AgentError::failed(
                "bad_config",
                "backend=api cannot use acp configuration",
            ));
        }
        let model = config
            .model
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                AgentError::failed("bad_config", "backend=api requires model or default_model")
            })?;
        let selector = config.provider.as_ref().map_or_else(
            || model.to_owned(),
            |provider| {
                if model.starts_with(&format!("{provider}/")) {
                    model.to_owned()
                } else {
                    format!("{provider}/{model}")
                }
            },
        );
        let reasoning = config
            .reasoning_effort
            .as_ref()
            .map(|value| serde_json::from_value::<ReasoningEffort>(json!(value)))
            .transpose()
            .map_err(|e| AgentError::failed("bad_config", e.to_string()))?;
        let cancel = CancellationToken::new();
        let kill = CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let sink = Arc::new(PetriEvents {
            sender:  ctx.logs.clone(),
            masker:  ctx.secrets.masker(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
            scope:   ctx.scope,
            node:    ctx.node.clone(),
        });
        let redactor = Arc::new(PetriRedactor(ctx.secrets.masker()));
        // Agent questions ride the same progress and control channels a human
        // gate uses, so the host's one interviewer answers both.
        let questions = Arc::new(AgentQuestions::new(
            ctx.logs.clone(),
            ctx.node.clone(),
            ctx.firing,
        ));
        let env = ctx.env.clone();
        let provider = questions.clone();
        let build = async {
            let environment = PebbleEnvironment::prepare(env, cancel.clone(), kill.clone())
                .await
                .map_err(|e| AgentError::failed("pebble_environment", e.to_string()))?;
            CodingAgent::builder(client.0.clone(), Arc::new(environment))
                .model(&selector)
                .options(
                    CodingAgentOptions::default()
                        .with_reasoning_effort(reasoning)
                        .with_memory_files(["AGENTS.md".into()])
                        .with_skill_dirs([".agents/skills".into(), ".pebble/skills".into()])
                        .with_wall_clock_timeout(Duration::from_millis(
                            config.timeout_ms.unwrap_or(86_400_000),
                        )),
                )
                .permission_level(PermissionLevel::Full)
                .event_sink(sink)
                .redactor(redactor)
                .human_input(provider)
                .build()
                .await
                .map_err(|e| AgentError::failed("pebble_config", e.to_string()))
        };
        tokio::pin!(build);
        let mut pending = Vec::new();
        let mut closed = false;
        let agent = loop {
            tokio::select! {
                result = &mut build => break match result { Err(_) if cancel.is_cancelled() => return Err(AgentError::Cancelled), other => other? },
                control = ctx.control.recv(), if !closed => {
                    match control {
                        Some(Control::Deliver(value)) if !cancel.is_cancelled() => {
                            if !questions.answer(&value) && let Some(text) = steering_text(&value) { pending.push(text); }
                        },
                        Some(Control::Kill) => { kill.cancel(); cancel.cancel(); },
                        Some(Control::Cancel) => cancel.cancel(),
                        None => { closed = true; cancel.cancel(); },
                        _ => {},
                    }
                }
            }
        };
        questions.set_session(agent.snapshot().session_id());
        let mut session = Self {
            agent,
            questions,
            cancel: cancel.clone(),
            kill: kill.clone(),
            _cancel_on_drop: guard,
            usage: TokenUsage::default(),
            cost: None,
            inference: Duration::ZERO,
            tool: Duration::ZERO,
            prompts: 0,
        };
        if cancel.is_cancelled() {
            session.shutdown(ShutdownReason::Cancelled).await?;
            return Err(AgentError::Cancelled);
        }
        for text in pending {
            session.agent.queue_follow_up(text);
        }
        Ok(session)
    }

    pub(crate) async fn prompt(
        &mut self,
        prompt: &str,
        control: &mut mpsc::Receiver<Control>,
    ) -> Result<String, AgentError> {
        let handle = self.agent.control_handle();
        let questions = self.questions.clone();
        let cancel = self.cancel.clone();
        let kill = self.kill.clone();
        let report = {
            let prompt = self.agent.prompt_with_cancellation(prompt, &cancel);
            tokio::pin!(prompt);
            let mut closed = false;
            loop {
                tokio::select! {
                    biased;
                    message = control.recv(), if !closed => match message {
                        Some(Control::Deliver(value)) if !cancel.is_cancelled() => {
                            if !questions.answer(&value) && let Some(text) = steering_text(&value) { handle.queue_follow_up(text); }
                        },
                        Some(Control::Kill) => { kill.cancel(); cancel.cancel(); },
                        Some(Control::Cancel) => cancel.cancel(),
                        None => { closed = true; cancel.cancel(); },
                        _ => {},
                    },
                    report = &mut prompt => break report,
                }
            }
        };
        self.record(&report);
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        report
            .result
            .map(|output| output.text.unwrap_or_default())
            .map_err(|e| AgentError::failed("pebble_prompt", e.to_string()))
    }

    fn record(&mut self, report: &PromptReport) {
        self.prompts += 1;
        self.usage = self.usage.saturating_add(report.usage);
        if let Some(cost) = report.cost_usd_micros {
            self.cost = Some(self.cost.unwrap_or(0).saturating_add(cost));
        }
        self.inference = self.inference.saturating_add(report.timing.inference);
        self.tool = self.tool.saturating_add(report.timing.tool);
    }

    pub(crate) fn metrics(&self) -> BTreeMap<SmolStr, Value> {
        BTreeMap::from([
            ("pebble.prompts".into(), json!(self.prompts)),
            ("pebble.usage".into(), json!(self.usage)),
            ("pebble.cost_usd_micros".into(), json!(self.cost)),
            (
                "pebble.inference_ms".into(),
                json!(elapsed_ms(self.inference)),
            ),
            ("pebble.tool_ms".into(), json!(elapsed_ms(self.tool))),
        ])
    }

    pub(crate) async fn shutdown(&mut self, reason: ShutdownReason) -> Result<(), AgentError> {
        self.agent
            .shutdown(reason)
            .await
            .map(|_| ())
            .map_err(|e| AgentError::failed("pebble_shutdown", e.to_string()))
    }
}

fn steering_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("text").and_then(Value::as_str))
        .map(str::to_owned)
}

struct PetriRedactor(Masker);
impl Redactor for PetriRedactor {
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Owned(self.0.mask(text))
    }
}

/// The driver's completion fence drains the progress channel before recording
/// the outcome. The nested envelope retains Pebble's event identity.
struct PetriEvents {
    sender:  mpsc::Sender<StepEvent>,
    masker:  Masker,
    firing:  FiringId,
    attempt: Attempt,
    scope:   ScopeId,
    node:    SmolStr,
}
#[async_trait::async_trait]
impl EventSink for PetriEvents {
    async fn record(&self, event: &CodingAgentEvent) -> Result<(), EventSinkError> {
        let value = serde_json::to_value(event)
            .map_err(|e| EventSinkError::new("Could not encode Pebble event").with_source(e))?;
        self.sender
            .send(StepEvent::Custom(
                json!({"kind": "pebble", "firing": self.firing, "attempt": self.attempt, "scope": self.scope, "node": self.node, "event": self.masker.mask_value(&value)}),
            ))
            .await
            .map_err(|_| EventSinkError::new("Petri event channel closed"))
    }
}
