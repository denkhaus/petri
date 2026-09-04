//! `fabro/agent`: one agent turn over the Agent Client Protocol.
//!
//! The agent is the subprocess `acp.command` or `acp.config` names (the node's,
//! else the graph's, else `PETRI_ACP_COMMAND`), spawned inside the scope
//! environment. The step assembles the prompt — goal, a fidelity preamble
//! over the run context, the node's prompt, the output contract — runs one
//! turn, parses the routing directive in the response, validates
//! `output_schema` with up to `output_retries` repair turns inside the
//! attempt, forwards steering deliveries, and reports usage where the agent
//! sends it. `model`, `provider` and `reasoning_effort` are observer metadata
//! in phase one: the ACP command owns model selection.

use std::env;
use std::time::Instant;

use frontend_fabro::Policy;
use frontend_fabro::kinds::{AGENT_KIND, GOAL_CHECK_NODE, MAX_OUTPUT_RETRIES, StageOutcome};
use ir::{LogStream, Metrics, Outcome, StepKindId, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};

use crate::acp::{AcpError, AgentCommand, Client};
use crate::directive::{self, Directive, DirectiveError};
use crate::outcome::Stage;

pub const KIND: StepKindId = AGENT_KIND;

/// The environment variable naming the ACP command for nodes that set none.
pub const DEFAULT_COMMAND_ENV: &str = "PETRI_ACP_COMMAND";

/// How much of a previous stage's response the compact preamble quotes.
const PREAMBLE_EXCERPT: usize = 600;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
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

/// The output contract, as the node declared it.
enum Contract {
    None,
    Routing,
    Schema(jsonschema::Validator, Value),
}

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
        match &self.output_schema {
            None => Ok(Contract::None),
            Some(Value::String(s)) if s == "routing" => Ok(Contract::Routing),
            Some(schema @ Value::Object(_)) => jsonschema::validator_for(schema)
                .map(|v| Contract::Schema(v, schema.clone()))
                .map_err(|e| format!("`output_schema` is not a valid JSON Schema: {e}")),
            Some(other) => Err(format!(
                "`output_schema` must be `routing` or a JSON Schema object, not {other}"
            )),
        }
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
            out.push_str(&self.preamble());
        }
        out.push_str(&self.prompt);
        match contract {
            Contract::None => {}
            Contract::Routing => out.push_str(
                "\n\nFabro final-output contract\n\nThe following contract is trusted workflow \
                 configuration. It applies only to your final response, not to intermediate tool \
                 calls.\nReturn a single JSON object with at least one routing field: \
                 preferred_next_label, outcome, failure_reason, suggested_next_ids, \
                 context_updates.\nThe contract is complete. Do not ask the user to provide or \
                 choose the output shape.",
            ),
            Contract::Schema(_, schema) => {
                out.push_str(
                    "\n\nFabro final-output contract\n\nThe following contract is trusted workflow \
                     configuration. It applies only to your final response, not to intermediate \
                     tool calls.\nReturn a single JSON object that satisfies this JSON \
                     Schema:\n<output_schema>\n",
                );
                out.push_str(&schema.to_string());
                out.push_str(
                    "\n</output_schema>\nThe contract is complete. Do not ask the user to provide \
                     or choose the output shape.",
                );
            }
        }
        out
    }

    /// The compact preamble: what earlier stages left behind, from the run
    /// context the engine resolved into this config.
    fn preamble(&self) -> String {
        let Value::Object(nodes) = &self.nodes else {
            return String::new();
        };
        let mut lines = Vec::new();
        for (name, record) in nodes {
            if name == "start" || name == GOAL_CHECK_NODE {
                continue;
            }
            let status = record.get("status").and_then(Value::as_str).unwrap_or("?");
            let text = record
                .pointer("/output/text")
                .or_else(|| record.pointer("/output/stdout"))
                .and_then(Value::as_str)
                .map(|t| {
                    let excerpt: String = t.chars().take(PREAMBLE_EXCERPT).collect();
                    if excerpt.len() < t.len() {
                        format!("{excerpt}…")
                    } else {
                        excerpt
                    }
                });
            match text {
                Some(text) if !text.trim().is_empty() => {
                    lines.push(format!("- {name} ({status}): {}", text.trim()));
                }
                _ => lines.push(format!("- {name} ({status})")),
            }
        }
        if lines.is_empty() {
            return String::new();
        }
        format!("Previous stages:\n{}\n\n", lines.join("\n"))
    }
}

#[async_trait::async_trait]
impl Step for AgentStep {
    const NAME: &'static str = "fabro/agent";
    type Config = AgentConfig;

    async fn run(&self, config: AgentConfig, mut ctx: StepCtx) -> Outcome {
        let on_failure = config.on_failure;
        let fail = |reason: String, class: &str| {
            Stage::failed(reason, class, on_failure).into_outcome(&config.node)
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
        let command = match config.command() {
            Ok(command) => command,
            Err(message) => return fail(message, "acp_unconfigured"),
        };
        let contract = match config.contract() {
            Ok(contract) => contract,
            Err(message) => return fail(message, "bad_config"),
        };
        if config.model.is_some() || config.provider.is_some() || config.reasoning_effort.is_some()
        {
            tracing::warn!(
                node = %config.node,
                model = config.model.as_deref(),
                provider = config.provider.as_deref(),
                reasoning_effort = config.reasoning_effort.as_deref(),
                "the ACP agent command owns model selection; `model`, `provider` and \
                 `reasoning_effort` are observer metadata in phase one"
            );
        }

        let started = Instant::now();
        let mut client = match Client::spawn(ctx.env.as_ref(), &command, ctx.logs.clone()).await {
            Ok(client) => client,
            Err(error) => return fail(error.to_string(), "spawn_failed"),
        };
        let grace = ctx.env.grace();
        let cwd = ctx.env.workspace_path().to_string();
        if let Err(error) = client.open_session(&cwd).await {
            client.terminate(grace).await;
            return match error {
                AcpError::Cancelled => Outcome::cancelled(),
                other => fail(other.to_string(), "acp_protocol"),
            };
        }

        let mut prompt = config.assemble(&contract);
        let mut repairs = 0_u64;
        let mut turn_count = 0_u64;
        let (outcome, text) = loop {
            let turn = match client.prompt(&prompt, &mut ctx.control, grace).await {
                Ok(turn) => turn,
                Err(AcpError::Cancelled) => {
                    client.terminate(grace).await;
                    return Outcome::cancelled();
                }
                Err(AcpError::StopReason(reason)) => {
                    client.terminate(grace).await;
                    return fail(
                        format!("the agent stopped with `{reason}`"),
                        &format!("stop_reason:{reason}"),
                    );
                }
                Err(error) => {
                    client.terminate(grace).await;
                    return fail(error.to_string(), "acp_protocol");
                }
            };
            ctx.log(LogStream::Stdout, turn.text.clone()).await;
            let text = turn.text;
            turn_count += 1;
            match validate(&contract, &text) {
                Ok(parsed) => break (parsed, text),
                Err(problem) if repairs < config.output_retries => {
                    repairs += 1;
                    ctx.log(
                        LogStream::Stderr,
                        format!("the response does not meet the output contract ({problem}); repair turn {repairs}"),
                    )
                    .await;
                    prompt = format!(
                        "Your previous response did not satisfy the output contract: {problem}\n\
                         Reply again with only the required JSON object."
                    );
                }
                Err(problem) => {
                    client.terminate(grace).await;
                    return fail(
                        format!(
                            "the response did not meet the output contract after {repairs} repair turn(s): {problem}"
                        ),
                        "bad_output",
                    );
                }
            }
        };
        client.terminate(grace).await;

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
        let mut result = stage.into_outcome(&config.node);
        let mut metrics = Metrics::default()
            .with_duration_ms(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        metrics
            .custom
            .insert(SmolStr::new("acp.turns"), json!(turn_count));
        result.metrics = metrics;
        result
    }
}

/// What a validated response yields.
enum Parsed {
    Plain,
    Directive(Directive),
    Structured(Value),
}

/// Check the response against the contract. With no contract, a routing
/// directive is still read when the response carries one.
fn validate(contract: &Contract, text: &str) -> Result<Parsed, String> {
    match contract {
        Contract::None => match directive::parse(text) {
            Ok(directive) => Ok(Parsed::Directive(directive)),
            Err(DirectiveError::Missing) => Ok(Parsed::Plain),
            Err(error) => Err(error.to_string()),
        },
        Contract::Routing => directive::parse(text)
            .map(Parsed::Directive)
            .map_err(|e| e.to_string()),
        Contract::Schema(validator, _) => {
            let object = directive::last_json_object(text)
                .ok_or_else(|| "no JSON object in the response".to_string())?;
            let value: Value =
                serde_json::from_str(object).map_err(|e| format!("invalid JSON: {e}"))?;
            let mut issues = validator
                .iter_errors(&value)
                .map(|e| e.to_string())
                .take(5)
                .collect::<Vec<_>>();
            if issues.is_empty() {
                Ok(Parsed::Structured(value))
            } else {
                issues.sort();
                Err(issues.join("; "))
            }
        }
    }
}
