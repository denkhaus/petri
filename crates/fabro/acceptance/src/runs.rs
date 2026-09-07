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
use execution::{CoordinatorEvent, CoordinatorRecord, ExecutionId, ExecutionObserver, InvocationId};
use fabro_steps::{Simulate, StubScripts, reported_outcome};
use frontend_fabro::BRANCH_META_KIND;
use frontend_fabro::kinds::GOAL_CHECK_NODE;
use ir::{FiringId, Graph, Value};
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

/// Script a node's stub in place: the `simulate` object the stub step reads.
/// Only for a graph that is not registered yet; a registered graph is named
/// by its digest, so a case's scripts travel as [`StubScripts`] instead.
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

/// One final record as the path observer saw it: a stage visit, or the
/// parent-side delegate of a parallel branch, which stands for the stages
/// its child invocation ran.
#[derive(Clone, Debug)]
struct Recorded {
    visit:  Visit,
    firing: FiringId,
    /// The branch index when the node is a branch delegate.
    branch: Option<u64>,
}

/// Every final record of every execution of the run, per execution and in
/// completion order, with the invocation tree from the lifecycle log, so the
/// path can be assembled across a `loop_restart` and across parallel branch
/// invocations.
#[derive(Default)]
struct PathObserver {
    records:     Mutex<BTreeMap<ExecutionId, Vec<Recorded>>>,
    /// How many history records of each execution were already read.
    seen:        Mutex<BTreeMap<ExecutionId, usize>>,
    /// Each execution's invocation.
    executions:  Mutex<BTreeMap<ExecutionId, InvocationId>>,
    /// Each child invocation's calling execution and firing.
    calls:       Mutex<BTreeMap<InvocationId, (ExecutionId, FiringId)>>,
    invocations: Mutex<BTreeMap<InvocationId, Vec<ExecutionId>>>,
}

impl PathObserver {
    /// The run's path: the root's records in order, with every branch
    /// delegate of one fork replaced by its child's stages in branch index
    /// order, so the path does not depend on which branch finished first.
    fn path(&self) -> Vec<Visit> {
        let records = self.records.lock().expect("not poisoned");
        let invocations = self.invocations.lock().expect("not poisoned");
        let calls = self.calls.lock().expect("not poisoned");
        let root = invocations
            .get(&InvocationId::ROOT)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for execution in root {
            self.assemble(execution, &records, &invocations, &calls, &mut out);
        }
        out
    }

    fn assemble(
        &self,
        execution: ExecutionId,
        records: &BTreeMap<ExecutionId, Vec<Recorded>>,
        invocations: &BTreeMap<InvocationId, Vec<ExecutionId>>,
        calls: &BTreeMap<InvocationId, (ExecutionId, FiringId)>,
        out: &mut Vec<Visit>,
    ) {
        let mut delegates: Vec<&Recorded> = Vec::new();
        let flush = |delegates: &mut Vec<&Recorded>, out: &mut Vec<Visit>| {
            delegates.sort_by_key(|record| record.branch);
            for delegate in delegates.drain(..) {
                let child = calls
                    .iter()
                    .find(|(_, (parent, firing))| *parent == execution && *firing == delegate.firing)
                    .map(|(child, _)| *child);
                let Some(child) = child else {
                    continue;
                };
                for child_execution in invocations.get(&child).cloned().unwrap_or_default() {
                    self.assemble(child_execution, records, invocations, calls, out);
                }
            }
        };
        for record in records.get(&execution).map(Vec::as_slice).unwrap_or_default() {
            if record.branch.is_some() {
                delegates.push(record);
                continue;
            }
            flush(&mut delegates, out);
            out.push(record.visit.clone());
        }
        flush(&mut delegates, out);
    }
}

impl ExecutionObserver for PathObserver {
    fn on_engine_record(&self, execution: ExecutionId, _record: &EventRecord, state: &EngineState) {
        let history = state.history();
        let mut seen = self.seen.lock().expect("not poisoned");
        let seen = seen.entry(execution).or_insert(0);
        let mut records = self.records.lock().expect("not poisoned");
        let known = records.entry(execution).or_default();
        if history.len() > *seen {
            for record in &history[*seen..] {
                let meta = state.graph().node(record.node).map(|node| &node.meta);
                let branch = meta
                    .filter(|meta| meta.get("kind").and_then(Value::as_str) == Some(BRANCH_META_KIND))
                    .and_then(|meta| meta["branch"]["index"].as_u64());
                // Synthetic nodes other than a branch delegate (the goal check,
                // a synthetic fan-in) are lowering artifacts, not stages.
                let synthetic = meta
                    .is_some_and(|meta| meta.get("synthetic") == Some(&Value::Bool(true)));
                if record.name == GOAL_CHECK_NODE || (synthetic && branch.is_none()) {
                    continue;
                }
                known.push(Recorded {
                    visit: Visit {
                        node:    record.name.to_string(),
                        outcome: reported_outcome(&record.outcome).as_str().to_string(),
                    },
                    firing: record.firing,
                    branch,
                });
            }
            *seen = history.len();
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        match &record.event {
            CoordinatorEvent::InvocationDeclared {
                invocation, call, ..
            } => {
                self.invocations
                    .lock()
                    .expect("not poisoned")
                    .entry(*invocation)
                    .or_default();
                if let Some(call) = call {
                    self.calls
                        .lock()
                        .expect("not poisoned")
                        .insert(*invocation, (call.parent, call.firing));
                }
            }
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } => {
                self.executions
                    .lock()
                    .expect("not poisoned")
                    .insert(*execution, *invocation);
                self.invocations
                    .lock()
                    .expect("not poisoned")
                    .entry(*invocation)
                    .or_default()
                    .push(*execution);
            }
            _ => {}
        }
    }
}

/// Run `graph` through the standalone host (so `loop_restart` successions
/// happen) and fold the report. Replay is verified by the runtime.
pub async fn run(graph: Graph, label: &str) -> RunResult {
    run_with_children(graph, Vec::new(), label).await
}

/// Run a root graph with every pre-lowered child workflow it may invoke.
pub async fn run_with_children(graph: Graph, children: Vec<Graph>, label: &str) -> RunResult {
    run_scripted(graph, children, StubScripts::default(), label).await
}

/// [`run_with_children`] with stub scripts handed to the runtime, so a
/// scripted stage inside a parallel branch's child graph is scripted too
/// without changing the registered graph.
pub async fn run_scripted(
    graph: Graph,
    children: Vec<Graph>,
    scripts: StubScripts,
    label: &str,
) -> RunResult {
    let dir = fresh_run_dir(label);
    let rt = stub_runtime(&dir).capability(scripts);
    let observer = Arc::new(PathObserver::default());
    let host_run = HostRun::new(graph)
        .with_children(children)
        .observe(observer.clone());
    let report = host::run_configured(&rt, host_run, |_, _| {})
        .await
        .unwrap_or_else(|e| panic!("the run completes: {e}"));
    let path = observer.path();
    let context = report
        .state
        .run_context()
        .kv
        .iter()
        // The bookkeeping keys the oracle harness drops from Fabro's context
        // too: the failure class and the fan-in's published results.
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "failure_class" | "parallel.results" | "parallel.branch_count"
            )
        })
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

    /// Lower the case: the root graph and every child graph its parallel
    /// branches run. Panics with the diagnostics when it does not lower.
    pub fn graphs(&self) -> (Graph, Vec<Graph>) {
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
        (graph, lowered.children)
    }

    /// The case's stub scripts as the runtime capability the stubs read.
    pub fn stub_scripts(&self) -> StubScripts {
        StubScripts(
            self.scripts
                .iter()
                .map(|(node, script)| {
                    let script: Simulate = serde_json::from_value(script.clone())
                        .unwrap_or_else(|e| panic!("case `{}` script `{node}`: {e}", self.name));
                    (node.clone(), script)
                })
                .collect(),
        )
    }

    /// Run the case under stubs, scripted as it declares.
    pub async fn run(&self) -> RunResult {
        let (graph, children) = self.graphs();
        run_scripted(graph, children, self.stub_scripts(), &self.name).await
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
