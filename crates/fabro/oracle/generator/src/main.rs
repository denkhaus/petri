//! Run every shared case through Fabro and record the result.
//!
//! Usage: `fabro-oracle-generator <cases dir> <expected dir> <fabro commit>`.
//! Each case's stubs are scripted the way Petri's stub steps are: the n-th
//! call of a node's handler takes the n-th `calls` entry, the last repeats.
//! The recorded shape is the one `fabro_acceptance::runs::RunResult` has —
//! status, the path of stage outcomes, the final public context, the number
//! of engine executions — so the two sides diff as JSON.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fabro_core::Context;
use fabro_graphviz::graph::{Graph, Node};
use fabro_types::run_event::EventBody;
use fabro_types::{FailureCategory, FailureDetail, RunId, StageOutcome, WorkflowSettings};
use fabro_workflow::error::Error;
use fabro_workflow::event::Emitter;
use fabro_workflow::handler::conditional::ConditionalHandler;
use fabro_workflow::handler::exit::ExitHandler;
use fabro_workflow::handler::fan_in::FanInHandler;
use fabro_workflow::handler::parallel::ParallelHandler;
use fabro_workflow::handler::start::StartHandler;
use fabro_workflow::handler::{Handler, HandlerRegistry};
use fabro_workflow::outcome::Outcome;
use fabro_workflow::run_options::RunOptions;
use fabro_workflow::services::EngineServices;
use fabro_workflow::test_support;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, Deserialize)]
struct Script {
    #[serde(default)]
    outcome:            Option<String>,
    #[serde(default)]
    failure_class:      Option<String>,
    #[serde(default)]
    failure_reason:     Option<String>,
    #[serde(default)]
    preferred_label:    Option<String>,
    #[serde(default)]
    suggested_next_ids: Vec<String>,
    #[serde(default)]
    context_updates:    BTreeMap<String, Value>,
    #[serde(default)]
    calls:              Vec<Script>,
}

#[derive(Deserialize)]
struct Case {
    name:      String,
    workflow:  String,
    #[serde(default)]
    scripts:   BTreeMap<String, Script>,
    #[serde(default)]
    departure: Option<String>,
}

/// The scripted stand-in for every stage handler.
struct Scripted {
    scripts: BTreeMap<String, Script>,
    calls:   Mutex<HashMap<String, usize>>,
    /// The context the last executed stage saw, plus its own updates: the
    /// closest thing to the run's final public context from inside a handler.
    last:    Mutex<BTreeMap<String, Value>>,
}

impl Scripted {
    fn script_for(&self, node: &str) -> Script {
        let mut script = self.scripts.get(node).cloned().unwrap_or_default();
        let mut calls = self.calls.lock().expect("not poisoned");
        let count = calls.entry(node.to_string()).or_insert(0);
        let call = *count;
        *count += 1;
        if !script.calls.is_empty() {
            script = script.calls[call.min(script.calls.len() - 1)].clone();
        }
        script
    }
}

#[async_trait]
impl Handler for Scripted {
    async fn execute(
        &self,
        node: &Node,
        context: &Context,
        graph: &Graph,
        _run_dir: &Path,
        _services: &EngineServices,
    ) -> Result<Outcome, Error> {
        let mut script = self.script_for(&node.id);
        let is_human = node.handler_type() == Some("human");
        let answers = script.outcome.as_deref().is_none_or(|o| o == "succeeded");
        if is_human && answers && script.preferred_label.is_none() && script.suggested_next_ids.is_empty()
        {
            // Fabro's dry run answers a gate with its first choice.
            let edges = graph.outgoing_edges(&node.id);
            if let Some(edge) = edges.iter().find(|e| !e.freeform()) {
                let label = edge.label().filter(|l| !l.is_empty()).unwrap_or(&edge.to);
                script.preferred_label = Some(label.to_string());
                script.suggested_next_ids = vec![edge.to.clone()];
                script
                    .context_updates
                    .entry("human.gate.selected".into())
                    .or_insert_with(|| json!(accelerator_key(label)));
                script
                    .context_updates
                    .entry("human.gate.label".into())
                    .or_insert_with(|| json!(label));
            } else if let Some(edge) = edges.first() {
                script.suggested_next_ids = vec![edge.to.clone()];
            }
        }
        let mut outcome = Outcome::success();
        let retry_requested = script.failure_class.as_deref() == Some("retry_requested");
        outcome.status = match script.outcome.as_deref().unwrap_or("succeeded") {
            "succeeded" => StageOutcome::Succeeded,
            "partially_succeeded" => StageOutcome::PartiallySucceeded,
            "skipped" => StageOutcome::Skipped,
            "failed" => {
                outcome.failure = Some(FailureDetail::new(
                    script
                        .failure_reason
                        .clone()
                        .unwrap_or_else(|| format!("[Scripted] {} failed", node.id)),
                    if retry_requested {
                        FailureCategory::TransientInfra
                    } else {
                        FailureCategory::Deterministic
                    },
                ));
                StageOutcome::Failed { retry_requested }
            }
            other => return Err(Error::Validation(format!("`{other}` is not a stage outcome"))),
        };
        outcome.preferred_label = script.preferred_label.clone();
        outcome.suggested_next_ids = script.suggested_next_ids.clone();
        for (key, value) in &script.context_updates {
            outcome.context_updates.insert(key.clone(), value.clone());
        }
        let mut snapshot: BTreeMap<String, Value> = context.snapshot().into_iter().collect();
        for (key, value) in &outcome.context_updates {
            snapshot.insert(key.clone(), value.clone());
        }
        *self.last.lock().expect("not poisoned") = snapshot;
        Ok(outcome)
    }
}

/// Fabro's accelerator key: `[K] label`, `K) label`, `K - label`, else the
/// first character.
fn accelerator_key(label: &str) -> String {
    let trimmed = label.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(end) = rest.find(']')
        && end > 0
    {
        return rest[..end].to_string();
    }
    if let Some(pos) = trimmed.find(')')
        && (1..=3).contains(&pos)
        && trimmed[..pos].chars().all(char::is_alphanumeric)
    {
        return trimmed[..pos].to_string();
    }
    if let Some(pos) = trimmed.find(" - ")
        && (1..=3).contains(&pos)
        && trimmed[..pos].chars().all(char::is_alphanumeric)
    {
        return trimmed[..pos].to_string();
    }
    trimmed.chars().next().map(|c| c.to_string()).unwrap_or_default()
}

/// Keys Fabro writes for its own bookkeeping, which Petri does not carry.
fn is_internal(key: &str) -> bool {
    key.starts_with("internal.")
        || key.starts_with("graph.")
        || key.starts_with("response.")
        || key.starts_with("thread.")
        || key.starts_with("current")
        || matches!(
            key,
            "outcome" | "failure_class" | "failure_signature" | "preferred_label" | "last_stage"
                | "last_response" | "command.output" | "parallel.results"
                | "parallel.branch_count"
        )
}

fn registry(scripted: Arc<Scripted>) -> HandlerRegistry {
    struct Shared(Arc<Scripted>);
    #[async_trait]
    impl Handler for Shared {
        async fn execute(
            &self,
            node: &Node,
            context: &Context,
            graph: &Graph,
            run_dir: &Path,
            services: &EngineServices,
        ) -> Result<Outcome, Error> {
            self.0.execute(node, context, graph, run_dir, services).await
        }
    }
    let mut registry = HandlerRegistry::new(Box::new(Shared(scripted.clone())));
    registry.register("start", Box::new(StartHandler));
    registry.register("exit", Box::new(ExitHandler));
    registry.register("conditional", Box::new(ConditionalHandler));
    registry.register("parallel", Box::new(ParallelHandler));
    registry.register("parallel.fan_in", Box::new(FanInHandler::new(None)));
    for kind in ["agent", "prompt", "human", "command", "wait"] {
        registry.register(kind, Box::new(Shared(scripted.clone())));
    }
    registry
}

fn run_options(run_dir: &Path, index: u64) -> RunOptions {
    RunOptions {
        settings:         WorkflowSettings::default(),
        run_dir:          run_dir.to_path_buf(),
        cancel_token:     Default::default(),
        run_id:           RunId::from(ulid::Ulid(0x0100_0000_0000_0000_0000_0000_0000_0000 + u128::from(index))),
        labels:           HashMap::new(),
        workflow_slug:    None,
        github_app:       None,
        pre_run_git:      None,
        fork_source_ref:  None,
        base_branch:      None,
        display_base_sha: None,
        git:              None,
    }
}

async fn run_case(case: &Case, index: u64, work: &Path) -> Value {
    let graph = fabro_graphviz::parser::parse(&case.workflow)
        .unwrap_or_else(|e| panic!("case `{}` does not parse: {e}", case.name));
    let scripted = Arc::new(Scripted {
        scripts: case.scripts.clone(),
        calls:   Mutex::new(HashMap::new()),
        last:    Mutex::new(BTreeMap::new()),
    });
    let run_dir = work.join(&case.name);
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir).expect("run dir");
    let sandbox: Arc<dyn fabro_agent::Sandbox> =
        Arc::new(fabro_agent::LocalSandbox::new(run_dir.clone()));
    let options = run_options(&run_dir, index);
    let emitter = Arc::new(Emitter::new(options.run_id));
    let events = test_support::collect_events(&emitter);
    let outcome = test_support::run_graph(registry(scripted.clone()), emitter, sandbox, &graph, &options).await;

    let mut path = Vec::new();
    let mut restarts = 0_u32;
    let mut completed = false;
    for event in events.lock().expect("not poisoned").iter() {
        match &event.body {
            EventBody::StageCompleted(props) => {
                if let Some(node) = &event.node_id {
                    path.push(json!({ "node": node, "outcome": props.status.to_string() }));
                }
            }
            EventBody::StageFailed(_) => {
                if let Some(node) = &event.node_id {
                    path.push(json!({ "node": node, "outcome": "failed" }));
                }
            }
            // A retried attempt is not a final record: Petri's history holds
            // only the final attempt of each firing, so the attempt this
            // retry follows leaves the path.
            EventBody::StageRetrying(_) => {
                if let Some(node) = &event.node_id
                    && path.last().is_some_and(|last| last["node"] == json!(node))
                {
                    path.pop();
                }
            }
            EventBody::LoopRestart(_) => restarts += 1,
            EventBody::RunCompleted(_) => completed = true,
            _ => {}
        }
    }
    let status = match &outcome {
        Ok(outcome) if outcome.status.is_successful() && completed => "success",
        _ => "failed",
    };
    let context: BTreeMap<String, Value> = scripted
        .last
        .lock()
        .expect("not poisoned")
        .iter()
        .filter(|(k, _)| !is_internal(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let _ = std::fs::remove_dir_all(&run_dir);
    json!({
        "status": status,
        "path": path,
        "context": context,
        "executions": restarts + 1,
    })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, cases_dir, expected_dir, commit] = args.as_slice() else {
        eprintln!("usage: fabro-oracle-generator <cases dir> <expected dir> <fabro commit>");
        std::process::exit(2);
    };
    let work = std::env::temp_dir().join("fabro-oracle");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(cases_dir)
        .expect("cases dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    std::fs::create_dir_all(expected_dir).expect("expected dir");
    for (index, path) in paths.iter().enumerate() {
        let text = std::fs::read_to_string(path).expect("case text");
        let case: Case = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let fabro = run_case(&case, index as u64 + 1, &work).await;
        let existing: Option<Value> = std::fs::read_to_string(Path::new(expected_dir).join(format!("{}.json", case.name)))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let mut fixture = json!({
            "fabro_commit": commit,
            "fabro": fabro,
        });
        if let Some(departure) = &case.departure {
            fixture["departure"] = json!(departure);
            // Petri's expected result for a departure is recorded by Petri's
            // own test (`PETRI_ORACLE_RECORD=1`); keep what it wrote.
            if let Some(petri) = existing.as_ref().and_then(|e| e.get("petri")) {
                fixture["petri"] = petri.clone();
            }
        }
        let out = Path::new(expected_dir).join(format!("{}.json", case.name));
        std::fs::write(&out, format!("{}\n", serde_json::to_string_pretty(&fixture).expect("json")))
            .expect("write fixture");
        println!("{}: {}", case.name, fabro["status"]);
    }
}
