//! Native Pebble sessions. The embedding application owns the model client.
//!
//! A session binds Petri's services for one node: the event sink attributed
//! to the node's firing and attempt, the question bridge, the redactor, the
//! tool-hook middleware, and the model controls. A node that continues a
//! retained thread resumes Pebble's warm export with a fresh set of those
//! bindings, so events, questions and hooks of the new stage are attributed
//! to the new stage; the predecessor was shut down after it exported. Per
//! stage metrics start at zero for each node.
//!
//! A session opens on a route ([`Resume::Fresh`]), continues a retained
//! export on the export's route ([`Resume::Export`]), or resumes a failed
//! session's record on another route ([`Resume::Failover`], Pebble's
//! `ResumeMode::UseModel`): the fallback chain in [`crate::fallback`] drives
//! the third. A prompt's model error stays typed ([`AgentError::Model`]) so
//! that chain can read it.

mod capture;
pub mod environment;
pub mod questions;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use environment::{PebbleEnvironment, elapsed_ms};
use execution::hooks::HookServiceHandle;
use executor::Masker;
use ir::{Attempt, Control, FiringId, ScopeId, StepEvent, Value};
use lithos_llm::Client;
use lithos_llm::catalog::Metadata;
use lithos_llm::types::{ReasoningEffort, Request, Speed};
use pebble_coding_agent::events::{
    CodingAgentEvent, EventSink, EventSinkError, PermissionLevel, TokenUsage,
};
use pebble_coding_agent::extensions::Redactor;
use pebble_coding_agent::state::SessionRecord;
use pebble_coding_agent::{
    CodingAgent, CodingAgentBuilder, CodingAgentExport, CodingAgentOptions, CodingInput,
    InputSource, PromptReport, ResumeMode, ShutdownReason,
};
use questions::AgentQuestions;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Steer, StepCtx};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::agent::AgentConfig;
use crate::agent::backend::AgentError;
use crate::fallback::{self, Disposition, Route};
use crate::hooks::tools::ToolHooks;
use crate::hooks::{self};
use crate::mcp::{self, McpServers};
use crate::{memory, skills};

/// Host capability supplied by applications embedding the native backend.
/// Construct the client with the application's catalog, credentials, and
/// retry policy. The capability owns the client on Petri's host.
#[derive(Clone)]
pub struct PebbleClient(pub Client);

/// Where a native session's conversation comes from.
pub(crate) enum Resume {
    /// A new conversation on `route`.
    Fresh(Route),
    /// A retained thread's warm export, on the export's own route.
    Export(Box<CodingAgentExport>),
    /// A failed session's record, continued on another route: the
    /// fallback chain's handoff.
    Failover {
        record: SessionRecord,
        route:  Route,
    },
}

/// One prompt turn's accounting, attributed to the route it ran on.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TurnUsage {
    pub usage:           Value,
    pub cost_usd_micros: Option<u64>,
    pub inference_ms:    u64,
    pub tool_ms:         u64,
}

pub(crate) struct NativeSession {
    agent:           CodingAgent,
    /// The node's MCP servers, shut down after the agent.
    mcp:             McpServers,
    questions:       Arc<AgentQuestions>,
    cancel:          CancellationToken,
    kill:            CancellationToken,
    _cancel_on_drop: DropGuard,
    usage:           TokenUsage,
    cost:            Option<u64>,
    inference:       Duration,
    tool:            Duration,
    prompts:         u64,
    last_turn:       Option<TurnUsage>,
}

/// The Pebble profile name (`anthropic`, `claude-5`, `openai`, `gpt56`,
/// `gpt6`, `gemini`, `kimi`) the model selector resolves to, from the
/// catalog's `metadata.pebble.profile` on the model then the provider. The
/// same rule Pebble applies when it builds the session.
pub fn profile_of(client: &Client, selector: &str) -> Option<String> {
    #[derive(serde::Deserialize, Default)]
    struct PebbleMetadata {
        profile: Option<String>,
    }
    let probe = Request::builder()
        .model(selector)
        .user("probe")
        .build()
        .ok()?;
    let route = client.resolve_route(&probe).ok()?;
    let read = |metadata: &Metadata| {
        metadata
            .namespace::<PebbleMetadata>("pebble")
            .ok()
            .flatten()
            .and_then(|m| m.profile)
    };
    read(route.model().metadata()).or_else(|| read(route.provider().metadata()))
}

/// Fabro's `speed` to lithos-llm's: `fast` asks for the fast tier,
/// `standard` for the balanced one.
pub fn speed_of(text: &str) -> Option<Speed> {
    match text {
        "fast" => Some(Speed::Fast),
        "standard" => Some(Speed::Balanced),
        _ => None,
    }
}

impl NativeSession {
    pub(crate) async fn open(
        config: &AgentConfig,
        ctx: &mut StepCtx,
        resume: Resume,
    ) -> Result<Self, AgentError> {
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
        let mut reasoning = config
            .reasoning_effort
            .as_ref()
            .map(|value| serde_json::from_value::<ReasoningEffort>(json!(value)))
            .transpose()
            .map_err(|e| AgentError::failed("bad_config", e.to_string()))?;
        let mut speed = match config.speed.as_deref() {
            None => None,
            Some(text) => Some(speed_of(text).ok_or_else(|| {
                AgentError::failed(
                    "bad_config",
                    format!(
                        "Invalid speed \"{text}\" for node \"{}\"; expected one of: standard, \
                         fast",
                        config.node
                    ),
                )
            })?),
        };
        // A routed open runs on the route's model and controls; an export
        // keeps its own route and takes the node's controls.
        let selector = match &resume {
            Resume::Fresh(route) | Resume::Failover { route, .. } => {
                reasoning = route.reasoning_effort;
                speed = route.speed;
                route.selector()
            }
            Resume::Export(_) => selector,
        };
        // Fabro's project documents for the model's profile, from the Git
        // root down to the working directory. Pebble loads them.
        let profile = profile_of(&client.0, &selector).unwrap_or_default();
        let memory_files = memory::select(
            ctx.env.as_ref(),
            &profile,
            memory::Scope::GitRootToWorkingDir,
        )
        .await;
        let hook_service = ctx.capability::<HookServiceHandle>();
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
        // The attempt's deadline is the driver's (`TimeoutPolicy::ExecutorEnforced`):
        // it counts active work only and cancels this session through `cancel`
        // when it expires. Pebble's own wall-clock timer stays unset, so it
        // cannot expire during an excluded interview wait.
        let env = ctx.env.clone();
        let provider = questions.clone();
        let tool_hooks = hook_service.map(|handle| {
            let view = hooks::step_view(ctx, "agent", &config.label, &config.kv);
            Arc::new(ToolHooks::new(
                handle.0.clone(),
                view,
                ctx.logs.clone(),
                ctx.node.clone(),
                ctx.firing,
                ctx.attempt,
            ))
        });
        let mcps = config.mcps.clone();
        let secrets = ctx.secrets.clone();
        let attribution = mcp::Attribution::of(ctx);
        // Fabro's skill directories, audited and recorded; Pebble discovers.
        let skills = skills::prepare(config, ctx).await;
        let build = async {
            // The node's MCP servers start first, on Petri's side of the
            // agent, so their tools are registered with the builder.
            let mut mcp =
                McpServers::start(&mcps, env.clone(), secrets, attribution, &cancel).await;
            let environment = PebbleEnvironment::prepare(env, cancel.clone(), kill.clone())
                .await
                .map_err(|e| AgentError::failed("pebble_environment", e.to_string()))?;
            let environment = Arc::new(environment);
            let options = CodingAgentOptions::default()
                .with_reasoning_effort(reasoning)
                .with_speed(speed)
                .with_max_tokens(config.max_tokens)
                .with_memory_files(memory_files)
                .with_skill_dirs(skills.paths());
            let options = fallback::configure_options(options);
            // A resumed export keeps its route and its conversation; a
            // failed session's record keeps the conversation and takes the
            // next route; the builder binds this node's services either way.
            let mut builder: CodingAgentBuilder = match resume {
                Resume::Export(export) => {
                    CodingAgent::resume_from_export(client.0.clone(), environment, *export)
                }
                Resume::Failover { record, route } => CodingAgent::resume(
                    client.0.clone(),
                    environment,
                    record,
                    ResumeMode::UseModel(route.selector()),
                ),
                Resume::Fresh(_) => {
                    CodingAgent::builder(client.0.clone(), environment).model(&selector)
                }
            };
            builder = builder
                .options(options)
                .permission_level(PermissionLevel::Full)
                .event_sink(sink)
                .redactor(redactor)
                .human_input(provider)
                .tools(mcp.tools());
            if let Some(middleware) = tool_hooks {
                builder = builder.tool_middleware(middleware);
            }
            match builder.build().await {
                Ok(agent) => Ok((agent, mcp)),
                Err(e) => {
                    mcp.shutdown().await;
                    Err(AgentError::failed("pebble_config", e.to_string()))
                }
            }
        };
        tokio::pin!(build);
        let mut pending = Vec::new();
        let mut closed = false;
        let (agent, mcp) = loop {
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
            mcp,
            questions,
            cancel: cancel.clone(),
            kill: kill.clone(),
            _cancel_on_drop: guard,
            usage: TokenUsage::default(),
            cost: None,
            inference: Duration::ZERO,
            tool: Duration::ZERO,
            prompts: 0,
            last_turn: None,
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

    /// One prompt turn. `agent_sourced` marks input the runtime wrote (the
    /// continuation after a failover), which Pebble attributes to the agent.
    /// A model error comes back typed as [`AgentError::Model`].
    pub(crate) async fn prompt(
        &mut self,
        prompt: &str,
        agent_sourced: bool,
        control: &mut mpsc::Receiver<Control>,
    ) -> Result<String, AgentError> {
        let handle = self.agent.control_handle();
        let questions = self.questions.clone();
        let cancel = self.cancel.clone();
        let kill = self.kill.clone();
        let input = if agent_sourced {
            CodingInput::text(prompt).with_source(InputSource::Agent)
        } else {
            CodingInput::text(prompt)
        };
        let report = {
            let prompt = self.agent.prompt_with_cancellation(input, &cancel);
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
        self.account(&report);
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        match report.result {
            Ok(output) => Ok(output.text.unwrap_or_default()),
            Err(error) => Err(match fallback::classify(&error) {
                Disposition::Cancelled => AgentError::Cancelled,
                Disposition::Model(failure) => AgentError::Model(failure),
                // A skill reference that does not expand keeps the skills
                // module's class and reason; any other agent error is what
                // the classifier said.
                Disposition::Other { class, message } => match &error {
                    pebble_coding_agent::Error::SkillExpansion(_) => {
                        AgentError::failed(skills::failure_class(&error), skills::describe(&error))
                    }
                    _ => AgentError::failed(class, message),
                },
            }),
        }
    }

    fn account(&mut self, report: &PromptReport) {
        self.prompts += 1;
        self.usage = self.usage.saturating_add(report.usage);
        if let Some(cost) = report.cost_usd_micros {
            self.cost = Some(self.cost.unwrap_or(0).saturating_add(cost));
        }
        self.inference = self.inference.saturating_add(report.timing.inference);
        self.tool = self.tool.saturating_add(report.timing.tool);
        self.last_turn = Some(TurnUsage {
            usage:           json!(report.usage),
            cost_usd_micros: report.cost_usd_micros,
            inference_ms:    elapsed_ms(report.timing.inference),
            tool_ms:         elapsed_ms(report.timing.tool),
        });
    }

    /// The last prompt turn's accounting.
    pub(crate) fn last_turn(&self) -> Option<TurnUsage> {
        self.last_turn.clone()
    }

    /// The durable record of the conversation so far, for a failover.
    pub(crate) fn record(&self) -> SessionRecord {
        self.agent.to_record()
    }

    pub(crate) fn session_id(&self) -> String {
        self.agent.id().to_owned()
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

    /// The conversation, warm, for a later node on the same thread.
    pub(crate) fn export(&self) -> CodingAgentExport {
        self.agent.export()
    }

    pub(crate) async fn shutdown(&mut self, reason: ShutdownReason) -> Result<(), AgentError> {
        let result = self
            .agent
            .shutdown(reason)
            .await
            .map(|_| ())
            .map_err(|e| AgentError::failed("pebble_shutdown", e.to_string()));
        // The servers outlive the agent's last tool call and nothing else.
        self.mcp.shutdown().await;
        result
    }
}

/// The guidance a delivered value carries: a core [`Steer`], or the older
/// bare string and `{ "text": ... }` spellings.
fn steering_text(value: &Value) -> Option<String> {
    if let Some(steer) = Steer::from_value(value) {
        return Some(steer.text);
    }
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
