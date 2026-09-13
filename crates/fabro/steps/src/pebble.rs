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
//! A session opens on a plan's route ([`Resume::Fresh`]) or continues a
//! retained export on the export's route with the plan the thread carries
//! ([`Resume::Export`]). Either way the plan's remaining routes are named to
//! Pebble as its fallback routes; Pebble moves the conversation when a model
//! error qualifies and reports the move on its own event stream, and the
//! session reads the route reached back for the thread a later node
//! continues. A prompt's model error stays typed ([`AgentError::Model`]) so
//! the stage reports its class.
//!
//! Pebble's `CodingAgentEvent` stream, recorded here as the `pebble`
//! envelope (the public `agent_activity`), is the contract for everything
//! Pebble knows: routes, MCP servers and their calls, tools, usage. The sink
//! restates none of it as a Petri event; it emits a `StepEvent::Custom` only
//! for a fact Pebble cannot know (a server never named to it,
//! [`mcp::UNAVAILABLE_EVENT`]; the skill directories' conventions), and puts
//! two of Pebble's facts on the node's stderr for the terminal: a fallback
//! move and a server that did not start.
//!
//! Text a host delivers to the node (`Control::Deliver`) that is not an
//! answer to the agent's question is a follow-up: it runs as its own user
//! turn once the current answer is reached. Deliveries ride Pebble's
//! steering bus, one per node run: text that arrives before the session is
//! built waits on the bus and reaches the session when it attaches, in the
//! same mode.

pub mod environment;
pub mod mcp;
pub mod questions;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use environment::{PebbleEnvironment, PortRoutes, elapsed_ms};
use execution::hooks::HookServiceHandle;
use executor::Masker;
use ir::{Attempt, Control, FiringId, LogStream, ScopeId, StepEvent, Value};
use lithos_llm::Client;
use lithos_llm::catalog::Metadata;
use lithos_llm::types::{ErrorKind, ReasoningEffort, Request, Speed};
use pebble_coding_agent::events::{
    AgentProfileKind, CodingAgentEvent, CodingEvent, EventSink, EventSinkError, PermissionLevel,
    TokenUsage,
};
use pebble_coding_agent::extensions::Redactor;
use pebble_coding_agent::steering::SteeringBus;
use pebble_coding_agent::{
    CodingAgent, CodingAgentBuilder, CodingAgentExport, CodingAgentOptions, CodingInput,
    MemoryDiscovery, PromptReport, ShutdownReason,
};
use questions::AgentQuestions;
use serde_json::json;
use smol_str::SmolStr;
use steps::{ProgressError, ProgressSender, Steer, StepCtx};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::agent::AgentConfig;
use crate::agent::backend::AgentError;
use crate::compaction::{self, CompactionPolicyHandle};
use crate::fallback::{self, Disposition, Plan};
use crate::hooks::tools::ToolHooks;
use crate::hooks::{self};
use crate::skills;
use crate::subagents::{self, Ledger};

/// Host capability supplied by applications embedding the native backend.
/// Construct the client with the application's catalog, credentials, and
/// retry policy. The capability owns the client on Petri's host.
#[derive(Clone)]
pub struct PebbleClient(pub Client);

/// Where a native session's conversation comes from. Either carries the
/// node's fallback plan at the route reached: Pebble runs the routes after
/// it.
pub(crate) enum Resume {
    /// A new conversation on the plan's current route.
    Fresh(Plan),
    /// A retained thread's warm export, on the export's own route, with the
    /// plan the thread carries.
    Export {
        export: Box<CodingAgentExport>,
        plan:   Plan,
    },
}

pub(crate) struct NativeSession {
    agent:           CodingAgent,
    /// The node's sink, kept for the node's name the bus keys on.
    events:          Arc<PetriEvents>,
    /// The node's fallback plan as the session was opened; the route
    /// reached is read off the agent's remaining routes.
    plan:            Plan,
    questions:       Arc<AgentQuestions>,
    /// The node run's steering bus, with this session attached under the
    /// node's name: every delivered follow-up goes through it.
    steering:        SteeringBus<SmolStr>,
    compaction:      compaction::Accounting,
    attribution:     compaction::Attribution,
    cancel:          CancellationToken,
    kill:            CancellationToken,
    _cancel_on_drop: DropGuard,
    /// What the session's children spent and did, for the metrics.
    subagents:       Arc<Ledger>,
    usage:           TokenUsage,
    cost:            Option<u64>,
    inference:       Duration,
    tool:            Duration,
    prompts:         u64,
}

/// The Pebble profile the model selector resolves to, from the catalog's
/// shared `metadata.agent.profile` on the model then the provider: the rule
/// Pebble applies when it builds a session. A prompt node, which builds no
/// session, asks here for the profile whose instruction files it reads.
pub fn profile_of(client: &Client, selector: &str) -> Option<AgentProfileKind> {
    #[derive(serde::Deserialize, Default)]
    struct AgentMetadata {
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
            .namespace::<AgentMetadata>("agent")
            .ok()
            .flatten()
            .and_then(|m| m.profile)
    };
    let named = read(route.model().metadata()).or_else(|| read(route.provider().metadata()))?;
    AgentProfileKind::ALL
        .iter()
        .copied()
        .find(|kind| kind.as_str() == named)
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
        let (selector, plan) = match &resume {
            Resume::Fresh(plan) => {
                let route = plan.current();
                reasoning = route.reasoning_effort;
                speed = route.speed;
                (route.selector(), plan.clone())
            }
            Resume::Export { plan, .. } => (selector, plan.clone()),
        };
        let hook_service = ctx.capability::<HookServiceHandle>();
        let compaction_policy = ctx.capability::<CompactionPolicyHandle>();
        let cancel = CancellationToken::new();
        let kill = CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        // Fabro's skill directories, in its order then the workflow's own;
        // Pebble discovers and reports, the sink attributes.
        let (skill_discovery, skill_labels) = skills::for_node(config, ctx);
        let events = Arc::new(PetriEvents {
            sender:  ctx.logs.clone(),
            masker:  ctx.secrets.masker(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
            scope:   ctx.scope,
            node:    ctx.node.clone(),
            skills:  skill_labels,
        });
        // The plan's remaining routes, for Pebble to fail over to in order,
        // each with its own controls; an export starts with none of its own.
        let fallback_routes = plan.pebble_routes(config.max_tokens);
        let ledger = Arc::new(Ledger::default());
        let sink = subagents::observe(events.clone(), ledger.clone());
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
        let session_events = events.clone();
        let build = async {
            let environment = PebbleEnvironment::prepare(env.clone(), cancel.clone(), kill.clone())
                .await
                .map_err(|e| AgentError::failed("pebble_environment", e.to_string()))?;
            let environment = Arc::new(environment);
            // The node's MCP servers: Fabro's entries as Pebble's, their
            // secrets resolved here and nowhere else. Pebble starts them while
            // it builds the agent and reports each one on its stream; a
            // server whose secret the run cannot supply is never named to
            // Pebble, so it is reported here. A send that fails here means
            // the driver stopped taking this attempt's progress; the build's
            // own outcome reports that.
            let servers = mcp::pebble_servers(&mcps, env.as_ref(), secrets.as_ref());
            for (server, error) in &servers.unavailable {
                let _ = session_events.unavailable(server, error).await;
            }
            // Fabro's project documents for the model's profile, from the
            // Git root down to the working directory: Pebble names the
            // files, does the walk, and loads them.
            let options = CodingAgentOptions::default()
                .with_reasoning_effort(reasoning)
                .with_speed(speed)
                .with_max_tokens(config.max_tokens)
                .with_memory_discovery(MemoryDiscovery::from_git_root())
                .with_skill_discovery(skill_discovery);
            let options = compaction::options(options, &config.compaction);
            // A resumed export keeps its route and its conversation; the
            // builder binds this node's services either way.
            let mut builder: CodingAgentBuilder = match resume {
                Resume::Export { export, .. } => {
                    CodingAgent::resume_from_export(client.0.clone(), environment, *export)
                }
                Resume::Fresh(_) => {
                    CodingAgent::builder(client.0.clone(), environment).model(&selector)
                }
            };
            builder = builder
                .options(options)
                .fallback_routes(fallback_routes)
                .permission_level(PermissionLevel::Full)
                .event_sink(sink)
                .redactor(redactor)
                .human_input(provider)
                .mcp_servers(servers.servers)
                .port_routes(Arc::new(PortRoutes::new(env)));
            builder = compaction::install(builder, compaction_policy);
            if let Some(middleware) = tool_hooks {
                builder = builder.tool_middleware(middleware);
            }
            builder = subagents::configure(builder, &config.subagents);
            // The chain, not the head alone: a refused event sink or a bad
            // model selector is the cause under Pebble's summary.
            builder
                .build()
                .await
                .map_err(|e| AgentError::failed("pebble_config", chain(&e)))
        };
        tokio::pin!(build);
        // Text delivered before the session exists waits on the bus, as a
        // follow-up, and reaches the session when it attaches below.
        let steering = SteeringBus::new();
        let mut closed = false;
        let agent = loop {
            tokio::select! {
                result = &mut build => break match result { Err(_) if cancel.is_cancelled() => return Err(AgentError::Cancelled), other => other? },
                control = ctx.control.recv(), if !closed => {
                    match control {
                        Some(Control::Deliver(value)) if !cancel.is_cancelled() => {
                            if !questions.answer(&value) && let Some(text) = steering_text(&value) { follow_up(&steering, text); }
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
        let attribution = compaction::Attribution {
            sender:  ctx.logs.clone(),
            node:    ctx.node.clone(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
        };
        let mut session = Self {
            compaction: compaction::Accounting::default(),
            attribution,
            agent,
            events,
            plan,
            questions,
            steering,
            cancel: cancel.clone(),
            kill: kill.clone(),
            _cancel_on_drop: guard,
            subagents: ledger,
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
        // The bus keys the attachment by the node; the control handle
        // follows the agent through any failover Pebble runs, so one
        // attachment serves the node run. Attaching drains what waited.
        let key = ctx.node.clone();
        if let Err(error) = session.steering.attach(
            key.clone(),
            session.session_id(),
            Arc::new(session.agent.control_handle()),
        ) {
            session.shutdown(ShutdownReason::Error).await?;
            return Err(AgentError::failed("pebble_config", error.to_string()));
        }
        let drained = session.steering.drain_pending_into(&key);
        if !drained.dropped.is_empty() {
            tracing::warn!(
                dropped = drained.dropped.len(),
                "follow-ups delivered before the session was built were dropped"
            );
        }
        Ok(session)
    }

    /// One prompt turn. A model error comes back typed as
    /// [`AgentError::Model`]. Pebble runs the plan's remaining routes inside
    /// the turn and reports each move on its stream.
    pub(crate) async fn prompt(
        &mut self,
        prompt: &str,
        control: &mut mpsc::Receiver<Control>,
    ) -> Result<String, AgentError> {
        let questions = self.questions.clone();
        let cancel = self.cancel.clone();
        let kill = self.kill.clone();
        let report = {
            let prompt = self
                .agent
                .prompt_with_cancellation(CodingInput::text(prompt), &cancel);
            tokio::pin!(prompt);
            let mut closed = false;
            loop {
                tokio::select! {
                    biased;
                    message = control.recv(), if !closed => match message {
                        Some(Control::Deliver(value)) if !cancel.is_cancelled() => {
                            if !questions.answer(&value) && let Some(text) = steering_text(&value) { follow_up(&self.steering, text); }
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
        let session = self.agent.snapshot().session_id().to_string();
        self.compaction
            .settle(&session, &report, &self.attribution)
            .await;
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
    }

    /// The fallback plan at the route reached: the routes Pebble has taken
    /// are the ones no longer among its remaining fallback routes.
    pub(crate) fn plan(&self) -> Plan {
        let mut plan = self.plan.clone();
        let taken = plan
            .remaining_routes()
            .len()
            .saturating_sub(self.agent.remaining_fallback_routes().len());
        for _ in 0..taken {
            plan.advance();
        }
        plan
    }

    pub(crate) fn session_id(&self) -> String {
        self.agent.id().to_owned()
    }

    pub(crate) fn metrics(&self) -> BTreeMap<SmolStr, Value> {
        let mut metrics = BTreeMap::from([
            ("pebble.prompts".into(), json!(self.prompts)),
            ("pebble.usage".into(), json!(self.usage)),
            ("pebble.cost_usd_micros".into(), json!(self.cost)),
            (
                "pebble.inference_ms".into(),
                json!(elapsed_ms(self.inference)),
            ),
            ("pebble.tool_ms".into(), json!(elapsed_ms(self.tool))),
            (subagents::METRIC.into(), self.subagents.metrics()),
        ]);
        metrics.extend(self.compaction.metrics());
        metrics
    }

    /// The conversation, warm, for a later node on the same thread.
    pub(crate) fn export(&self) -> CodingAgentExport {
        self.agent.export()
    }

    pub(crate) async fn shutdown(&mut self, reason: ShutdownReason) -> Result<(), AgentError> {
        // The session leaves the bus first: a delivery that lands during
        // the shutdown waits there and is dropped with the bus, as the
        // agent could not run it.
        self.steering.detach(&self.events.node, &self.session_id());
        // Pebble closes the MCP servers it started with the agent, after its
        // last tool call and before this returns, so before the node returns
        // and the scope's environment is released.
        self.agent
            .shutdown(reason)
            .await
            .map(|_| ())
            .map_err(|e| AgentError::failed("pebble_shutdown", e.to_string()))
    }
}

/// An error and its source chain on one line, for the places that must
/// flatten a typed error into a recorded message.
fn chain(error: &dyn Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
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

/// Queue delivered text on the node run's bus as a follow-up: on the
/// attached session, to run as its own turn once the current answer is
/// reached, or on the bus itself while no session is attached yet. A full
/// queue evicts its oldest message; the bus reports what it dropped.
fn follow_up(bus: &SteeringBus<SmolStr>, text: String) {
    let delivery = bus.follow_up(text.into());
    if !delivery.dropped.is_empty() {
        tracing::warn!(
            dropped = delivery.dropped.len(),
            "a follow-up queue was full; the oldest follow-up was dropped"
        );
    }
}

struct PetriRedactor(Masker);
impl Redactor for PetriRedactor {
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Owned(self.0.mask(text))
    }
}

/// Pebble's event sink over the attempt's progress channel. Each event is
/// sent acknowledged: `record` returns only after the driver appended the
/// `StepProgress` record and every durable store confirmed the write, so an
/// acknowledgement to Pebble means the event is in Petri's log, and a write
/// failure stops the prompt as Pebble's contract asks. Queueing alone would not
/// do: the driver's completion fence drains the channel before it records the
/// outcome, which orders the events ahead of the outcome, but a crash after an
/// unacknowledged send can lose the event. What a crash still repeats is the
/// attempt: an attempt whose finish never landed is re-dispatched on resume
/// and emits its events again, so a record may appear twice; Pebble's
/// `(stream_id, seq)` in the nested envelope is the idempotency key.
///
/// Beside the envelope, the sink emits Petri's own events only for what
/// Pebble cannot know: the conventions behind the skill directories Pebble
/// reports having searched, and a server never named to Pebble because its
/// secret is unavailable ([`PetriEvents::unavailable`]). Two of Pebble's
/// facts also go to the node's stderr, for the terminal: a fallback move and
/// a server that did not start ([`stderr_line`]).
struct PetriEvents {
    sender:  ProgressSender,
    masker:  Masker,
    firing:  FiringId,
    attempt: Attempt,
    scope:   ScopeId,
    node:    SmolStr,
    /// What names the skill directories Pebble reports having searched.
    skills:  skills::Labels,
}

impl PetriEvents {
    /// A server Petri never named to Pebble because a secret its entry
    /// needs is unavailable: the line on the node's stderr, then
    /// [`mcp::UNAVAILABLE_EVENT`], both acknowledged.
    async fn unavailable(&self, server: &str, error: &str) -> Result<(), ProgressError> {
        self.sender
            .send_acked(StepEvent::Log {
                stream: LogStream::Stderr,
                line:   self
                    .masker
                    .mask(&format!("mcp server `{server}` failed to start: {error}")),
            })
            .await?;
        self.sender
            .send_acked(StepEvent::Custom(self.masker.mask_value(&json!({
                "kind": mcp::UNAVAILABLE_EVENT,
                "node": self.node,
                "firing": self.firing,
                "attempt": self.attempt,
                "server": server,
                "error": error,
            }))))
            .await
    }
}

/// What a person watching the node's stderr should read of `event`: a
/// fallback move, or a server that did not start. Both facts stay Pebble's
/// on the event stream; the line is for the terminal.
fn stderr_line(event: &CodingEvent) -> Option<String> {
    match event {
        CodingEvent::RouteFailover {
            from,
            to,
            attempt,
            error,
            ..
        } => Some(format!(
            "model fallback: {from} failed ({}); continuing on {to} (attempt {attempt} of the plan)",
            error.llm_kind.as_ref().map_or("error", ErrorKind::as_str)
        )),
        CodingEvent::McpServerFailed { server, error, .. } => {
            Some(format!("mcp server `{server}` failed to start: {error}"))
        }
        _ => None,
    }
}

#[async_trait::async_trait]
impl EventSink for PetriEvents {
    async fn record(&self, event: &CodingAgentEvent) -> Result<(), EventSinkError> {
        let value = serde_json::to_value(event)
            .map_err(|e| EventSinkError::new("Could not encode Pebble event").with_source(e))?;
        self.sender
            .send_acked(StepEvent::Custom(
                json!({"kind": "pebble", "firing": self.firing, "attempt": self.attempt, "scope": self.scope, "node": self.node, "event": self.masker.mask_value(&value)}),
            ))
            .await
            .map_err(|error| EventSinkError::new(error.to_string()).with_source(error))?;
        if let Some(line) = stderr_line(&event.event) {
            self.sender
                .send_acked(StepEvent::Log {
                    stream: LogStream::Stderr,
                    line:   self.masker.mask(&line),
                })
                .await
                .map_err(|error| EventSinkError::new(error.to_string()).with_source(error))?;
        }
        // The directories Pebble searched are Petri's record, with the
        // convention behind each; a skipped file or directory is Pebble's
        // report and the diagnostic is Petri's.
        let at = skills::Attribution {
            node:    self.node.clone(),
            firing:  self.firing,
            attempt: self.attempt,
        };
        if let Some(dirs) = skills::labeled(event, &self.skills) {
            skills::record_resolved(&self.sender, &at, self.scope, &dirs).await;
        }
        let skipped = skills::skipped(event);
        if !skipped.is_empty() {
            skills::report(&self.sender, &at, &skipped).await;
        }
        Ok(())
    }
}
