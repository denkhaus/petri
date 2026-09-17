//! Model resolution at admission: the [`AdmissionPass`] that pins every
//! LLM node's route when the runtime has a catalog.
//!
//! Fabro resolves every model selector to a concrete provider and model
//! against the eligible providers when a run is created, and persists the
//! result. Petri does the same at `Runtime::check`, through this pass, when
//! the `PebbleClient` capability is installed: every `attractor/agent` node
//! on the API backend and every `attractor/prompt` node gets the plan the
//! stage would have built at its first firing ([`crate::fallback`]), written
//! on its config as a [`FrozenPlan`] under [`PLAN_KEY`], with `model` and
//! `provider` replaced by the original route's concrete values and the
//! `[run.model.fallbacks]` table removed. The `start` stage's copy of the
//! table is checked here and removed too, so the stage does not check a
//! table already admitted against a catalog that may have changed since.
//! The node's `meta` keeps the selectors as written: it is the frontend's
//! display record.
//!
//! The graph a run persists is the resolved one, so dispatch and resume run
//! the admitted routes whatever the catalog says later. A selector that
//! resolves to nothing refuses the graph with [`UNKNOWN_CODE`]; a fallback
//! table that is malformed, names a provider as a key, or keys two chains
//! to one model refuses it with [`FALLBACKS_CODE`]. Without the capability
//! the pass does nothing and the stages resolve their selectors themselves.
//! A nested workflow the workflow step lowers during the run is not
//! admitted here; its stages resolve at firing time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use frontend_attractor::fallbacks::CONFIG_KEY as FALLBACKS_KEY;
use frontend_attractor::kinds::{AGENT_KIND, PROMPT_KIND, STAGE_KIND};
use ir::{Graph, Value};
use lithos_llm::Client;
use runtime::{AdmissionPass, AdmissionProblem};
use serde_json::{Map, json};
use steps::Capabilities;

use crate::agent::AgentBackend;
use crate::fallback::{self, ConfigError, FrozenPlan, PlanError, Requested, Resolved};
use crate::pebble::PebbleClient;

/// The diagnostic code for a selector the catalog cannot resolve: a model,
/// a provider, a provider with no default model, or a fallback chain's key
/// or reference that names something the catalog does not know.
pub const UNKNOWN_CODE: &str = "attractor.model.unknown";

/// The diagnostic code for a `[run.model.fallbacks]` table the run cannot
/// use whatever the catalog: a reference that does not parse, a key that
/// names a provider or a provider-qualified model, or two keys that resolve
/// to one model.
pub const FALLBACKS_CODE: &str = "attractor.model.fallbacks";

/// The config key the frozen plan rides on an agent or prompt node.
pub const PLAN_KEY: &str = "plan";

/// The pass [`crate::register`] installs.
#[derive(Clone, Copy, Debug, Default)]
pub struct ModelAdmission;

impl AdmissionPass for ModelAdmission {
    fn admit(&self, graph: &mut Graph, caps: &Capabilities) -> Vec<AdmissionProblem> {
        match caps.get::<PebbleClient>() {
            Some(client) => resolve_graph(&client.0, graph),
            None => Vec::new(),
        }
    }
}

/// Resolve every LLM node of `graph` against `client`, in node order, and
/// report what could not be resolved. The graph is changed as far as the
/// pass got; a graph with problems is withheld by the caller.
pub fn resolve_graph(client: &Client, graph: &mut Graph) -> Vec<AdmissionProblem> {
    let mut tables = Tables::new(client);
    for node in &mut graph.body.nodes {
        let name = node.name.clone();
        let Some(config) = node.step.config.as_object_mut() else {
            continue;
        };
        if node.step.kind == STAGE_KIND {
            if config.get("kind").and_then(Value::as_str) == Some("start") {
                let table = config.remove(FALLBACKS_KEY).unwrap_or(Value::Null);
                tables.check(&table);
            }
        } else if node.step.kind == PROMPT_KIND
            || (node.step.kind == AGENT_KIND && backend_of(config) == AgentBackend::Api)
        {
            tables.resolve_node(&name, config);
        }
    }
    tables.problems
}

fn backend_of(config: &Map<String, Value>) -> AgentBackend {
    config
        .get("backend")
        .cloned()
        .map(serde_json::from_value::<AgentBackend>)
        .and_then(Result::ok)
        .unwrap_or_default()
}

/// The chains resolved once per distinct table, and the problems found so
/// far. A table's problem is reported once, however many nodes carry it.
struct Tables<'a> {
    client:   &'a Client,
    resolved: HashMap<String, Arc<Result<Resolved, ConfigError>>>,
    reported: HashSet<String>,
    problems: Vec<AdmissionProblem>,
}

impl<'a> Tables<'a> {
    fn new(client: &'a Client) -> Self {
        Self {
            client,
            resolved: HashMap::new(),
            reported: HashSet::new(),
            problems: Vec::new(),
        }
    }

    /// Check the `start` stage's copy of the table.
    fn check(&mut self, table: &Value) {
        if table.is_null() {
            return;
        }
        let _ = self.resolve_table(table);
    }

    /// The chains of `table`, resolved, or `None` after reporting why not.
    fn resolve_table(&mut self, table: &Value) -> Option<Arc<Result<Resolved, ConfigError>>> {
        let key = table.to_string();
        let chains: BTreeMap<String, Vec<String>> = match serde_json::from_value(table.clone()) {
            Ok(chains) => chains,
            Err(error) => {
                if self.reported.insert(key) {
                    self.problems.push(AdmissionProblem::new(
                        FALLBACKS_CODE,
                        None,
                        format!("`run.model.fallbacks` does not parse: {error}"),
                    ));
                }
                return None;
            }
        };
        let client = self.client;
        let resolved = self
            .resolved
            .entry(key.clone())
            .or_insert_with(|| Arc::new(fallback::resolve(client, &chains)))
            .clone();
        if let Err(error) = resolved.as_ref()
            && self.reported.insert(key)
        {
            self.problems.push(AdmissionProblem::new(
                code_of(error),
                None,
                error.to_string(),
            ));
        }
        Some(resolved)
    }

    /// Resolve one agent or prompt node: its chains, its provider default,
    /// its route, its plan. The config is rewritten only when everything
    /// resolved.
    fn resolve_node(&mut self, node: &str, config: &mut Map<String, Value>) {
        let table = config.remove(FALLBACKS_KEY).unwrap_or(Value::Null);
        let resolved = if table.is_null() {
            Arc::new(Ok(Resolved::default()))
        } else {
            match self.resolve_table(&table) {
                Some(resolved) => resolved,
                None => return,
            }
        };
        let Ok(resolved) = resolved.as_ref() else {
            return;
        };
        let provider = config
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut model = config
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // A provider with no model runs the provider's default model, as
        // the stage fills it in.
        if let Err(message) =
            fallback::fill_provider_default(self.client, &mut model, provider.as_deref())
        {
            self.problems.push(AdmissionProblem::new(
                UNKNOWN_CODE,
                Some(node),
                format!("node `{node}`: {message}"),
            ));
            return;
        }
        // A node with no model and no provider, or with controls that do
        // not parse, is the stage's `bad_config` refusal, as it is today;
        // admission adds no refusal of its own for those.
        let Some(model) = model.filter(|s| !s.trim().is_empty()) else {
            return;
        };
        let reasoning_effort = config.get("reasoning_effort").and_then(Value::as_str);
        let speed = config.get("speed").and_then(Value::as_str);
        let Ok((reasoning_effort, speed)) = fallback::controls_of(node, reasoning_effort, speed)
        else {
            return;
        };
        let requested = Requested {
            provider: provider.as_deref(),
            model: &model,
            reasoning_effort,
            speed,
        };
        let frozen = match FrozenPlan::from_resolved(self.client, resolved, &requested) {
            Ok(frozen) => frozen,
            Err(PlanError::Config(error)) => {
                self.problems.push(AdmissionProblem::new(
                    code_of(&error),
                    Some(node),
                    error.to_string(),
                ));
                return;
            }
            Err(PlanError::Primary { selector }) => {
                self.problems.push(AdmissionProblem::new(
                    UNKNOWN_CODE,
                    Some(node),
                    format!(
                        "node `{node}` names `{selector}`, which no available provider offers; \
                         set `model` and `provider` to a catalog model on an enabled provider"
                    ),
                ));
                return;
            }
        };
        config.insert("model".into(), Value::String(frozen.original.model.clone()));
        config.insert(
            "provider".into(),
            Value::String(frozen.original.provider.clone()),
        );
        config.insert(PLAN_KEY.into(), json!(frozen));
    }
}

/// The code a fallback table's refusal carries: [`UNKNOWN_CODE`] when it
/// names something the catalog does not know, [`FALLBACKS_CODE`] for a
/// structural problem.
fn code_of(error: &ConfigError) -> &'static str {
    match error {
        ConfigError::KeyUnknown { .. } | ConfigError::UnknownProvider { .. } => UNKNOWN_CODE,
        ConfigError::KeyNamesProvider { .. }
        | ConfigError::KeyNamesQualifiedModel { .. }
        | ConfigError::KeyConflict { .. }
        | ConfigError::BadReference { .. } => FALLBACKS_CODE,
    }
}
