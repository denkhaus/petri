//! Run a lowered Fabro graph under the stub registry, on the host executor,
//! with byte-identical replay, and read the result the way the plan's
//! acceptance items do: the path taken, each node's Fabro outcome, the final
//! `kv`, and the run status.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs, process};

use execution::host::{self, HostRun};
use execution::{CoordinatorRecord, ExecutionId, ExecutionObserver};
use fabro_steps::reported_outcome;
use frontend_fabro::kinds::GOAL_CHECK_NODE;
use ir::{Graph, Value};
use runtime::engine::{EngineState, EventRecord};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde::{Deserialize, Serialize};

/// The standard runtime plus the Fabro stub registry, over `run_dir`.
pub fn stub_runtime(run_dir: &Path) -> Runtime {
    let mut options = RunOptions::new(run_dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    fabro_steps::register_stubs(Runtime::standard()).options(options)
}

/// A fresh run directory under the system temp dir.
pub fn fresh_run_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir()
        .join("petri-fabro")
        .join(format!("{label}-{}-{n}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the temp dir for this run is creatable");
    dir
}

/// Script a node's stub: the `simulate` object the stub step reads.
pub fn simulate(graph: &mut Graph, node: &str, script: Value) {
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == node)
        .unwrap_or_else(|| panic!("node `{node}`"));
    let Value::Object(config) = &mut node.step.config else {
        panic!("`{}` has no object config to script", node.name);
    };
    config.insert("simulate".into(), script);
}

/// One node's final record, in Fabro's vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visit {
    pub node:    String,
    pub outcome: String,
}

/// What a run did, in the terms the Fabro oracle also records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    /// `success`, `failed` or `cancelled`.
    pub status:     String,
    /// Every final record, in completion order. Synthetic nodes (`goal_check`)
    /// are left out so the path is the Fabro path.
    pub path:       Vec<Visit>,
    /// The final run context, minus the bookkeeping keys the steps write.
    pub context:    BTreeMap<String, Value>,
    /// How many engine executions the run took: 1, or more after a
    /// `loop_restart`.
    pub executions: u32,
}

impl RunResult {
    pub fn nodes(&self) -> Vec<&str> {
        self.path.iter().map(|v| v.node.as_str()).collect()
    }
}

/// Every final record of every execution of the run, in completion order —
/// the run's path across a `loop_restart`, which no single execution report
/// holds.
#[derive(Default)]
struct PathObserver {
    visits: Mutex<Vec<Visit>>,
    seen:   Mutex<BTreeMap<ExecutionId, usize>>,
}

impl ExecutionObserver for PathObserver {
    fn on_engine_record(&self, execution: ExecutionId, _record: &EventRecord, state: &EngineState) {
        let history = state.history();
        let mut seen = self.seen.lock().expect("not poisoned");
        let known = seen.entry(execution).or_insert(0);
        if history.len() > *known {
            let mut visits = self.visits.lock().expect("not poisoned");
            for record in &history[*known..] {
                if record.name != GOAL_CHECK_NODE {
                    visits.push(Visit {
                        node:    record.name.to_string(),
                        outcome: reported_outcome(&record.outcome).as_str().to_string(),
                    });
                }
            }
            *known = history.len();
        }
    }

    fn on_lifecycle(&self, _record: &CoordinatorRecord) {}
}

/// Run `graph` through the standalone host (so `loop_restart` successions
/// happen) and fold the report. Replay is verified by the runtime.
pub async fn run(graph: Graph, label: &str) -> RunResult {
    run_with_children(graph, Vec::new(), label).await
}

/// Run a root graph with every pre-lowered child workflow it may invoke.
pub async fn run_with_children(graph: Graph, children: Vec<Graph>, label: &str) -> RunResult {
    let dir = fresh_run_dir(label);
    let rt = stub_runtime(&dir);
    let observer = Arc::new(PathObserver::default());
    let host_run = HostRun::new(graph)
        .with_children(children)
        .observe(observer.clone());
    let report = host::run_configured(&rt, host_run, |_, _| {})
        .await
        .unwrap_or_else(|e| panic!("the run completes: {e}"));
    let path = observer.visits.lock().expect("not poisoned").clone();
    let context = report
        .state
        .run_context()
        .kv
        .iter()
        .filter(|(k, _)| k.as_str() != "failure_class")
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let executions = report
        .state
        .start()
        .map_or(1, |start| start.execution_index + 1);
    let status = report.status.to_string();
    let _ = fs::remove_dir_all(&dir);
    RunResult {
        status,
        path,
        context,
        executions,
    }
}

/// A shared scripted case: the workflow text and, per node, what its stub
/// returns. The oracle generator runs the same cases through Fabro.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Case {
    pub name:      String,
    pub workflow:  String,
    #[serde(default)]
    pub scripts:   BTreeMap<String, Value>,
    /// Why Petri's result is expected to differ from Fabro's, when it is.
    #[serde(default)]
    pub departure: Option<String>,
}

impl Case {
    pub fn load(path: &Path) -> Self {
        let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Lower and script the case. Panics with the diagnostics when it does not
    /// lower.
    pub fn graph(&self) -> Graph {
        let lowered = frontend_fabro::load_text(&format!("{}.fabro", self.name), &self.workflow);
        let graph = lowered.graph.unwrap_or_else(|| {
            panic!(
                "case `{}` does not lower:\n{}",
                self.name,
                lowered
                    .diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
        let mut graph = graph;
        for (node, script) in &self.scripts {
            simulate(&mut graph, node, script.clone());
        }
        graph
    }
}

/// The Markdown run sweep report.
pub fn runs_report(results: &[(String, RunResult)], pin: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Fabro corpus run sweep\n");
    let _ = writeln!(
        out,
        "Every corpus workflow that lowers, run end to end on the host executor under the stub \
         registry (Fabro's `--dry-run` outcomes: every stage succeeds, a human gate takes its \
         first choice), with byte-identical replay verified. Fabro at `{pin}`. Generated by \
         `cargo nextest run -p petri-fabro-acceptance --test runs`.\n"
    );
    let ok = results
        .iter()
        .filter(|(_, r)| r.status == "success")
        .count();
    let _ = writeln!(out, "| Result | Count |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| reached exit | {ok} |");
    let _ = writeln!(out, "| ended without exit | {} |", results.len() - ok);
    let _ = writeln!(out, "\n| File | Status | Stages | Path |");
    let _ = writeln!(out, "|---|---|---|---|");
    for (file, result) in results {
        let _ = writeln!(
            out,
            "| `{file}` | {} | {} | {} |",
            result.status,
            result.path.len(),
            result.nodes().join(" → ")
        );
    }
    out
}

/// The `RunResult` as the oracle records it, for a fixture diff.
pub fn result_json(result: &RunResult) -> Value {
    serde_json::to_value(result).expect("RunResult is serializable")
}
