//! `fabro/agent`: ACP or native Pebble, with shared prompt assembly,
//! output validation, repair turns, and routing.

pub(crate) mod backend;
use std::env;
use std::time::{Duration, Instant};

pub use backend::AgentBackend;
use backend::{AgentError, Session};
use frontend_fabro::Policy;
use frontend_fabro::kinds::{AGENT_KIND, MAX_OUTPUT_RETRIES, StageOutcome};
use ir::{LogStream, Metrics, Outcome, StepKindId, Value};
use pebble_coding_agent::ShutdownReason;
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};

use crate::acp::AgentCommand;
use crate::blobs::{self, OutputStore};
use crate::contract::{Contract, Parsed, repair_message, validate};
use crate::outcome::{ExplicitRoutes, Stage};
use crate::preamble;

pub const KIND: StepKindId = AGENT_KIND;

/// The environment variable naming the ACP command for nodes that set none.
pub const DEFAULT_COMMAND_ENV: &str = "PETRI_ACP_COMMAND";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub backend:          AgentBackend,
    pub label:            String,
    pub node:             String,
    #[serde(default)]
    pub kind:             Option<String>,
    #[serde(default)]
    pub goal:             String,
    #[serde(default)]
    pub prompt:           String,
    #[serde(default)]
    pub model:            Option<String>,
    #[serde(default)]
    pub provider:         Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub fidelity:         Option<String>,
    #[serde(default)]
    pub output_schema:    Option<Value>,
    #[serde(default = "default_output_retries")]
    pub output_retries:   u64,
    #[serde(default)]
    pub acp:              Option<Value>,
    #[serde(default)]
    pub on_failure:       Option<Policy>,
    /// The node's explicit routes, for failure promotion.
    #[serde(default, rename = "routes")]
    pub explicit_routes:  Option<ExplicitRoutes>,
    #[serde(default)]
    pub timeout_ms:       Option<u64>,
    #[serde(default)]
    pub kv:               Value,
    #[serde(default)]
    pub nodes:            Value,
}

fn default_output_retries() -> u64 {
    2
}

pub struct AgentStep;

impl AgentConfig {
    fn command(&self) -> Result<AgentCommand, String> {
        if let Some(acp) = &self.acp {
            if let Some(line) = acp.get("command").and_then(Value::as_str) {
                return AgentCommand::from_command_line(line);
            }
            if let Some(config) = acp.get("config") {
                return AgentCommand::from_config(config);
            }
        }
        match env::var(DEFAULT_COMMAND_ENV) {
            Ok(line) if !line.trim().is_empty() => AgentCommand::from_command_line(&line),
            _ => Err(format!(
                "node `{}` names no ACP agent: set `acp.command` or `acp.config` on the node or \
                 the graph, or {DEFAULT_COMMAND_ENV} in the environment",
                self.node
            )),
        }
    }

    fn contract(&self) -> Result<Contract, String> {
        Contract::from_config(self.output_schema.as_ref())
    }

    /// The prompt as the agent receives it.
    fn assemble(&self, contract: &Contract) -> String {
        let mut out = String::new();
        if !self.goal.is_empty() {
            out.push_str("Goal: ");
            out.push_str(&self.goal);
            out.push_str("\n\n");
        }
        let fidelity = self.fidelity.as_deref().unwrap_or("compact");
        if fidelity != "truncate" {
            out.push_str(&preamble::previous_stages(&self.nodes));
        }
        out.push_str(&self.prompt);
        out.push_str(&contract.prompt_suffix());
        out
    }
}

#[async_trait::async_trait]
impl Step for AgentStep {
    const NAME: &'static str = "fabro/agent";
    type Config = AgentConfig;

    async fn run(&self, config: AgentConfig, mut ctx: StepCtx) -> Outcome {
        let on_failure = config.on_failure;
        let routes = config.explicit_routes.clone();
        let kv = config.kv.clone();
        let fail = |reason: String, class: &str| {
            Stage::failed(reason, class, on_failure)
                .with_routing(routes.clone(), kv.clone())
                .into_outcome(&config.node)
        };
        if config.output_retries > MAX_OUTPUT_RETRIES {
            return fail(
                format!(
                    "`output_retries={}` exceeds the hard maximum of {MAX_OUTPUT_RETRIES}",
                    config.output_retries
                ),
                "bad_config",
            );
        }
        let contract = match config.contract() {
            Ok(contract) => contract,
            Err(message) => return fail(message, "bad_config"),
        };
        let started = Instant::now();
        let mut session = match Session::open(&config, &mut ctx).await {
            Ok(session) => session,
            Err(AgentError::Cancelled) => return Outcome::cancelled(),
            Err(AgentError::Failed { class, message }) => return fail(message, &class),
        };
        let mut turns = 0;
        let result = run_session(&config, &contract, &mut ctx, &mut session, &mut turns).await;
        let reason = match &result {
            Ok(_) => ShutdownReason::Completed,
            Err(AgentError::Cancelled) => ShutdownReason::Cancelled,
            Err(_) => ShutdownReason::Error,
        };
        let shutdown = session.shutdown(reason, ctx.env.grace()).await;
        let result = match result {
            Ok(stage) => shutdown.map(|()| stage),
            Err(error) => Err(error),
        };
        let mut outcome = match result {
            Ok(mut stage) => {
                if let Some(store) = ctx.capability::<OutputStore>() {
                    blobs::offload_updates(&mut stage.context_updates, store.0.as_ref()).await;
                }
                stage
                    .with_routing(routes.clone(), kv.clone())
                    .into_outcome(&config.node)
            }
            Err(AgentError::Cancelled) => Outcome::cancelled(),
            Err(AgentError::Failed { class, message }) => fail(message, &class),
        };
        outcome.metrics = Metrics {
            duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            custom: session.metrics(turns),
            ..Metrics::default()
        };
        outcome
    }
}

async fn run_session(
    config: &AgentConfig,
    contract: &Contract,
    ctx: &mut StepCtx,
    session: &mut Session,
    turn_count: &mut u64,
) -> Result<Stage, AgentError> {
    let mut prompt = config.assemble(contract);
    let mut repairs = 0_u64;
    let (outcome, text) = loop {
        let text = session
            .prompt(
                &prompt,
                &mut ctx.control,
                ctx.env.grace(),
                config.timeout_ms.map(Duration::from_millis),
            )
            .await?;
        ctx.log(LogStream::Stdout, text.clone()).await;
        *turn_count += 1;
        match validate(contract, &text) {
            Ok(parsed) => break (parsed, text),
            Err(problem) if repairs < config.output_retries => {
                repairs += 1;
                ctx.log(
                        LogStream::Stderr,
                        format!("the response does not meet the output contract ({problem}); repair turn {repairs}"),
                    )
                    .await;
                prompt = repair_message(&problem);
            }
            Err(problem) => {
                return Err(AgentError::failed(
                    "bad_output",
                    format!(
                        "the response did not meet the output contract after {repairs} repair turn(s): {problem}"
                    ),
                ));
            }
        }
    };

    let mut stage = Stage::new(StageOutcome::Succeeded, config.on_failure);
    stage.output.insert("text".into(), json!(text));
    stage.output.insert("turns".into(), json!(turn_count));
    stage.context_updates.insert(
        SmolStr::new(format!("response.{}", config.node)),
        json!(text),
    );
    stage
        .context_updates
        .insert(SmolStr::new("last_response"), json!(text));
    stage
        .context_updates
        .insert(SmolStr::new("last_stage"), json!(config.node));
    match outcome {
        Parsed::Directive(directive) => directive.apply_to(&mut stage),
        Parsed::Structured(value) => {
            stage.output.insert("structured".into(), value.clone());
            stage
                .context_updates
                .insert(SmolStr::new(format!("output.{}", config.node)), value);
        }
        Parsed::Plain => {}
    }
    Ok(stage)
}
