//! `fabro/prompt`: Fabro's prompt node (`tab`), and a fan-in with a
//! `prompt`. One model call through the application's `lithos-llm` client,
//! with no agent tools and no coding-agent loop. Petri owns the prompt
//! assembly, the output contract, the repair turns and the result mapping;
//! `lithos-llm` owns the provider transport.
//!
//! Fabro's prompt handler sends the fidelity preamble and the node's prompt
//! as one user message, then validates the response against the node's
//! `output_schema`. A response that misses the contract gets a repair turn:
//! the failed response and the repair message are appended and the model is
//! asked again, up to `output_retries` times. The result writes
//! `last_stage`, `last_response` (the first 200 characters) and
//! `response.<node>`, then the routing fields or `output.<node>`.
//!
//! A prompted fan-in is the same stage with the ordered branch results in
//! its prompt: Fabro renders `parallel.results` into the preamble's context
//! section; here the results ride the fan-in's inputs and are rendered the
//! same way, with each branch's id, status and updates.
//!
//! The step emits two `StepEvent::Custom` payloads a host can map onto
//! Fabro's `stage.prompt` and `prompt.completed` events, each with a stable
//! `kind`: [`PROMPT_EVENT`] before the first model call carries the rendered
//! prompt, the model selector and the fan-in sources; [`COMPLETED_EVENT`]
//! after the last call carries the response text, the outcome, the usage,
//! the cost, the number of calls and the duration.

use std::time::{Duration, Instant};

use frontend_fabro::Policy;
use frontend_fabro::kinds::{PROMPT_KIND, StageOutcome};
use ir::{Control, LogStream, Metrics, Outcome, StepEvent, StepKindId, Value};
use lithos_llm::types::{
    Message, ReasoningEffort, Request, Response, ResponseFormat, Role, TokenCounts,
};
use lithos_llm::{Client, Error};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};
use tokio::sync::mpsc;

use crate::agent::AgentBackend;
use crate::blobs::{self, OutputStore};
use crate::contract::{Contract, Parsed, repair_message, validate};
use crate::outcome::{ExplicitRoutes, Stage};
use crate::pebble::PebbleClient;
use crate::preamble;

pub const KIND: StepKindId = PROMPT_KIND;

/// How much of the response `last_response` keeps, Fabro's truncation.
const LAST_RESPONSE_CHARS: usize = 200;

/// The `kind` of the `StepEvent::Custom` payload emitted before the first
/// model call: `{ kind, node, firing, attempt, model, prompt, sources }`.
pub const PROMPT_EVENT: &str = "fabro.prompt";

/// The `kind` of the `StepEvent::Custom` payload emitted after the last
/// model call: `{ kind, node, firing, attempt, model, outcome, response,
/// calls, repairs, usage, cost_usd_micros, duration_ms }`.
pub const COMPLETED_EVENT: &str = "fabro.prompt.completed";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    /// A prompt node is API-only; the lowering leaves `backend` out unless a
    /// node names one, so the default here is the only one it can run on.
    #[serde(default = "api_backend")]
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
    pub on_failure:       Option<Policy>,
    #[serde(default, rename = "routes")]
    pub explicit_routes:  Option<ExplicitRoutes>,
    #[serde(default)]
    pub timeout_ms:       Option<u64>,
    #[serde(default)]
    pub kv:               Value,
    /// The fidelity preamble a parallel branch was forked with, rendered by
    /// the branch step from the parent's stage records. Present only inside
    /// a branch child; it stands in for the child's own (empty) records.
    #[serde(default)]
    pub preamble:         Option<String>,
    /// A `for_each` branch's item, rendered as fenced untrusted data by the
    /// branch step. Appended after the prompt, as Fabro appends it.
    #[serde(default)]
    pub item_data:        Option<String>,
    #[serde(default)]
    pub nodes:            Value,
    /// A prompted fan-in: the branch source node ids, in edge order.
    #[serde(default)]
    pub sources:          Vec<String>,
    /// A prompted fan-in: the ordered branch results the barrier collected.
    #[serde(default)]
    pub branch_results:   Value,
}

fn default_output_retries() -> u64 {
    2
}

fn api_backend() -> AgentBackend {
    AgentBackend::Api
}

pub struct PromptStep;

impl PromptConfig {
    /// The model selector `lithos-llm` resolves: `provider/model` when a
    /// provider qualifies the model.
    fn selector(&self) -> Result<String, String> {
        let model = self
            .model
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "prompt node `{}` names no model: set `model`, the graph's `default_model`, \
                     or `[run.model] name` in workflow.toml",
                    self.node
                )
            })?;
        Ok(self.provider.as_ref().map_or_else(
            || model.to_owned(),
            |provider| {
                if model.starts_with(&format!("{provider}/")) {
                    model.to_owned()
                } else {
                    format!("{provider}/{model}")
                }
            },
        ))
    }

    /// The one user message: the fidelity preamble, the branch results for
    /// a fan-in, the node's prompt, and the contract.
    fn assemble(&self, contract: &Contract, results: &Value) -> String {
        let mut out = String::new();
        if !self.goal.is_empty() {
            out.push_str("Goal: ");
            out.push_str(&self.goal);
            out.push_str("\n\n");
        }
        let fidelity = self.fidelity.as_deref().unwrap_or("compact");
        if fidelity != "truncate" {
            match &self.preamble {
                Some(preamble) => out.push_str(preamble),
                None => out.push_str(&preamble::previous_stages(&self.nodes)),
            }
        }
        if !self.sources.is_empty() {
            out.push_str(&preamble::branch_results(&self.sources, results));
        }
        out.push_str(&self.prompt);
        if let Some(item) = self.item_data.as_deref().filter(|item| !item.is_empty()) {
            out.push_str("\n\n");
            out.push_str(item);
        }
        out.push_str(&contract.prompt_suffix());
        out
    }

    fn response_format(contract: &Contract) -> Option<ResponseFormat> {
        match contract {
            Contract::None => None,
            Contract::Routing => Some(ResponseFormat::JsonObject),
            Contract::Schema(_, schema) => Some(ResponseFormat::JsonSchema {
                name:   "fabro_output".to_string(),
                schema: schema.clone(),
            }),
        }
    }
}

#[async_trait::async_trait]
impl Step for PromptStep {
    const NAME: &'static str = "fabro/prompt";
    type Config = PromptConfig;

    async fn run(&self, config: PromptConfig, mut ctx: StepCtx) -> Outcome {
        let on_failure = config.on_failure;
        let routes = config.explicit_routes.clone();
        let kv = config.kv.clone();
        let fail = |reason: String, class: &str| {
            Stage::failed(reason, class, on_failure)
                .with_routing(routes.clone(), kv.clone())
                .into_outcome(&config.node)
        };
        if config.backend == AgentBackend::Acp {
            return fail(
                "backend=\"acp\" is only valid on agent nodes; prompt nodes are API-only".into(),
                "bad_config",
            );
        }
        let Some(client) = ctx.capability::<PebbleClient>() else {
            return fail(
                "a prompt node requires the application's model client (PebbleClient capability)"
                    .into(),
                "pebble_unconfigured",
            );
        };
        let contract = match Contract::from_config(config.output_schema.as_ref()) {
            Ok(contract) => contract,
            Err(message) => return fail(message, "bad_config"),
        };
        let selector = match config.selector() {
            Ok(selector) => selector,
            Err(message) => return fail(message, "bad_config"),
        };
        let reasoning = match config
            .reasoning_effort
            .as_ref()
            .map(|value| serde_json::from_value::<ReasoningEffort>(json!(value)))
            .transpose()
        {
            Ok(reasoning) => reasoning,
            Err(error) => return fail(error.to_string(), "bad_config"),
        };
        let store = ctx.capability::<OutputStore>();
        let mut results = match &store {
            Some(store) => blobs::hydrate(config.branch_results.clone(), store.0.as_ref()).await,
            None => config.branch_results.clone(),
        };
        crate::parallel::strip_placeholders(&mut results);
        let started = Instant::now();
        let prompt = config.assemble(&contract, &results);
        let _ = ctx
            .logs
            .send(StepEvent::Custom(json!({
                "kind": PROMPT_EVENT,
                "node": config.node,
                "firing": ctx.firing,
                "attempt": ctx.attempt,
                "model": selector,
                "prompt": prompt,
                "sources": config.sources,
            })))
            .await;
        let completed = |outcome: &str,
                         response: Option<&str>,
                         calls: u64,
                         repairs: u64,
                         usage: &TokenCounts,
                         cost: Option<u64>| {
            StepEvent::Custom(json!({
                "kind": COMPLETED_EVENT,
                "node": config.node,
                "firing": ctx.firing,
                "attempt": ctx.attempt,
                "model": selector,
                "outcome": outcome,
                "response": response,
                "calls": calls,
                "repairs": repairs,
                "usage": usage,
                "cost_usd_micros": cost,
                "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }))
        };
        let mut messages = vec![Message::text(Role::User, prompt)];
        let mut usage = TokenCounts::default();
        let mut cost: Option<u64> = None;
        let mut turns = 0_u64;
        let mut repairs = 0_u64;
        let (parsed, text) = loop {
            let mut request = Request::builder().model(&selector);
            for message in &messages {
                request = request.message(message.clone());
            }
            if let Some(effort) = reasoning {
                request = request.reasoning_effort(effort);
            }
            // Fabro asks the provider for a JSON response shape when the node
            // has a contract. A model whose catalog row does not offer that
            // shape still answers; the contract is then enforced by the
            // validation below alone, as it is for an agent node.
            if let Some(format) = PromptConfig::response_format(&contract)
                && supports_format(&client.0, &selector, &format)
            {
                request = request.response_format(format);
            }
            if let Some(ms) = config.timeout_ms {
                request = request.timeout(Duration::from_millis(ms));
            }
            let request = match request.build() {
                Ok(request) => request,
                Err(error) => return fail(error.to_string(), "bad_config"),
            };
            let response = match complete(&client.0, request, &mut ctx.control).await {
                Ok(Some(response)) => response,
                Ok(None) => return Outcome::cancelled(),
                Err(error) => {
                    let class = format!("llm:{:?}", error.kind()).to_lowercase();
                    let _ = ctx
                        .logs
                        .send(completed("failed", None, turns, repairs, &usage, cost))
                        .await;
                    let mut outcome = fail(error.to_string(), &class);
                    outcome.metrics = metrics(started, turns, &usage, cost);
                    return outcome;
                }
            };
            turns += 1;
            usage = add_usage(usage, response.usage);
            if let Some(response_cost) = &response.cost {
                cost = Some(cost.unwrap_or(0).saturating_add(response_cost.usd_micros));
            }
            let text = response.text();
            ctx.log(LogStream::Stdout, text.clone()).await;
            match validate(&contract, &text) {
                Ok(parsed) => break (parsed, text),
                Err(problem) if repairs < config.output_retries => {
                    repairs += 1;
                    ctx.log(
                        LogStream::Stderr,
                        format!(
                            "the response does not meet the output contract ({problem}); repair \
                             turn {repairs}"
                        ),
                    )
                    .await;
                    messages.push(Message::text(Role::Assistant, text));
                    messages.push(Message::text(Role::User, repair_message(&problem)));
                }
                Err(problem) => {
                    let _ = ctx
                        .logs
                        .send(completed(
                            "failed",
                            Some(&text),
                            turns,
                            repairs,
                            &usage,
                            cost,
                        ))
                        .await;
                    let mut outcome = fail(
                        format!(
                            "output schema validation failed after {repairs} repair attempt(s): \
                             {problem}"
                        ),
                        "bad_output",
                    );
                    outcome.metrics = metrics(started, turns, &usage, cost);
                    return outcome;
                }
            }
        };

        let mut stage = Stage::new(StageOutcome::Succeeded, on_failure)
            .with_routing(routes.clone(), kv.clone());
        stage.output.insert("text".into(), json!(text));
        stage.output.insert("turns".into(), json!(turns));
        stage.output.insert("model".into(), json!(selector));
        if !config.sources.is_empty() {
            stage.output.insert("sources".into(), json!(config.sources));
            stage.output.insert(
                "branch_count".into(),
                json!(results.as_array().map_or(0, Vec::len)),
            );
            // The prompted fan-in is the barrier too: it publishes the
            // results the plain fan-in would have.
            stage.context_updates.insert(
                SmolStr::new(crate::parallel::RESULTS_KEY),
                results.clone(),
            );
            stage.context_updates.insert(
                SmolStr::new(crate::parallel::BRANCH_COUNT_KEY),
                json!(results.as_array().map_or(0, Vec::len)),
            );
        }
        stage.context_updates.insert(
            SmolStr::new(format!("response.{}", config.node)),
            json!(text),
        );
        let excerpt: String = text.chars().take(LAST_RESPONSE_CHARS).collect();
        stage
            .context_updates
            .insert(SmolStr::new("last_response"), json!(excerpt));
        stage
            .context_updates
            .insert(SmolStr::new("last_stage"), json!(config.node));
        match parsed {
            Parsed::Directive(directive) => directive.apply_to(&mut stage),
            Parsed::Structured(value) => {
                stage.output.insert("structured".into(), value.clone());
                stage
                    .context_updates
                    .insert(SmolStr::new(format!("output.{}", config.node)), value);
            }
            Parsed::Plain => {}
        }
        if let Some(store) = &store {
            blobs::offload_updates(&mut stage.context_updates, store.0.as_ref()).await;
        }
        let _ = ctx
            .logs
            .send(completed(
                stage.outcome.as_str(),
                Some(&text),
                turns,
                repairs,
                &usage,
                cost,
            ))
            .await;
        let mut outcome = stage.into_outcome(&config.node);
        outcome.metrics = metrics(started, turns, &usage, cost);
        outcome
    }
}

/// Whether the catalog row the selector resolves to offers `format`. A
/// selector that does not resolve is left to the call, which reports why.
fn supports_format(client: &Client, selector: &str, format: &ResponseFormat) -> bool {
    let Ok(probe) = Request::builder().model(selector).user("probe").build() else {
        return true;
    };
    match client.resolve_route(&probe) {
        Ok(route) => !route
            .model()
            .capabilities()
            .response_format(format)
            .is_unsupported(),
        Err(_) => true,
    }
}

/// One model call, cancel-aware: a `Cancel` or `Kill` on the control channel
/// abandons the request (`None`); a `Deliver` is ignored, a prompt has no
/// session to steer.
async fn complete(
    client: &Client,
    request: Request,
    control: &mut mpsc::Receiver<Control>,
) -> Result<Option<Response>, Error> {
    let call = client.complete(request);
    tokio::pin!(call);
    let mut closed = false;
    loop {
        tokio::select! {
            result = &mut call => return result.map(Some),
            message = control.recv(), if !closed => match message {
                Some(Control::Cancel | Control::Kill) => return Ok(None),
                None => closed = true,
                Some(_) => {}
            },
        }
    }
}

fn add_usage(total: TokenCounts, next: TokenCounts) -> TokenCounts {
    TokenCounts {
        input:       total.input.saturating_add(next.input),
        output:      total.output.saturating_add(next.output),
        reasoning:   total.reasoning.saturating_add(next.reasoning),
        cache_read:  total.cache_read.saturating_add(next.cache_read),
        cache_write: total.cache_write.saturating_add(next.cache_write),
    }
}

fn metrics(started: Instant, turns: u64, usage: &TokenCounts, cost: Option<u64>) -> Metrics {
    Metrics {
        duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        custom: [
            (SmolStr::new("prompt.calls"), json!(turns)),
            (SmolStr::new("prompt.usage"), json!(usage)),
            (SmolStr::new("prompt.cost_usd_micros"), json!(cost)),
        ]
        .into_iter()
        .collect(),
        ..Metrics::default()
    }
}
