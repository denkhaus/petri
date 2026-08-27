//! The GHA end-to-end harness: lower with the real frontend, run on the standard
//! runtime.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use frontend_gha::load;
use runtime::driver::RunReport;
use runtime::executor::Retention;
use runtime::frontend::{FileSource, MapFiles, NoFiles};
use runtime::ir::Graph;
use runtime::{RunOptions, Runtime, engine, ir};
use serde_json::json;

pub fn files(pairs: &[(&str, &str)]) -> MapFiles {
    MapFiles(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

pub fn lower_ok(text: &str) -> Graph {
    lower_ok_with(text, &NoFiles)
}

pub fn lower_ok_with(text: &str, files: &dyn FileSource) -> Graph {
    let lowered = load(".github/workflows/test.yml", text, files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("expected a graph")
}

/// Run parameters a real host would supply.
pub fn with_params(graph: Graph) -> Graph {
    let mut graph = graph;
    graph.params.entry("github".into()).or_insert(json!({
        "sha": "0123456789abcdef", "ref": "refs/heads/main", "ref_name": "main",
        "repository": "example/repo", "actor": "tester", "event_name": "push",
        "run_id": "1", "run_number": "1",
    }));
    graph.params.entry("runner".into()).or_insert(json!({
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "name": "local",
    }));
    graph.params.entry("vars".into()).or_insert(json!({}));
    graph
}

fn run_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("petri-gha")
        .join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The standard runtime plus the GitHub step kinds the frontend lowers to — what
/// the distribution registers, assembled here because a component's tests may not
/// depend on the distribution.
fn runtime(dir: &std::path::Path) -> Runtime {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(1);
    options.retention = Retention::Never;
    Runtime::standard()
        .options(options)
        .step(github_actions::RunStep)
        .step(github_actions::ActionStep)
}

/// Run on the standard runtime, which verifies replay itself.
pub async fn run_host(graph: Graph, label: &str) -> RunReportPlus {
    let graph = with_params(graph);
    let dir = run_dir(label);
    let report = runtime(&dir)
        .run(graph)
        .await
        .expect("replay is byte-identical");
    let _ = std::fs::remove_dir_all(&dir);
    RunReportPlus::from(report)
}

/// Start a run, cancel it once `node` has started, and return the report.
///
/// The step must print something once it is under way (`echo ready && sleep 30`):
/// the cancel is triggered by its log file appearing. Replay byte-identity is
/// verified on the way out, cancellation included.
pub async fn run_host_then_cancel(graph: Graph, label: &str, node: &str) -> (RunReportPlus, ()) {
    let graph = with_params(graph);
    let original = graph.clone();
    let dir = run_dir(label);
    let driver = runtime(&dir).driver(graph);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    // Wait for the named step's log file to appear, then cancel.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let logs = dir.join("logs");
    let want = node.replace('/', "_");
    loop {
        let seen = std::fs::read_dir(&logs)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with(&want))
            })
            .unwrap_or(false);
        if seen || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = run.await.expect("run finished");
    let _ = std::fs::remove_dir_all(&dir);
    testkit::assert_replay_identical(&original, &report);
    (RunReportPlus::from(report), ())
}

/// A report in the shape the tests read.
pub struct RunReportPlus {
    pub status: ir::RunStatus,
    pub state: engine::EngineState,
    pub commands: Vec<engine::Command>,
}

impl From<RunReport> for RunReportPlus {
    fn from(r: RunReport) -> Self {
        Self {
            status: r.status,
            state: r.state,
            commands: Vec::new(),
        }
    }
}

pub fn started(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .log
        .records()
        .iter()
        .filter_map(|r| match &r.event {
            engine::Event::StepStarted { firing, .. } => Some(*firing),
            _ => None,
        })
        .filter_map(|firing| {
            report
                .state
                .history()
                .iter()
                .find(|h| h.firing == firing)
                .map(|h| h.name.to_string())
        })
        .collect()
}

pub fn status_of(report: &RunReportPlus, name: &str) -> Option<String> {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome.status.tag().to_string())
}

pub fn log_lines(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgress {
                ev: ir::StepEvent::Log { line, .. },
                ..
            } => Some(line.clone()),
            _ => None,
        })
        .collect()
}
