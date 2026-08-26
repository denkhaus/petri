#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use driver::{Driver, RunConfig, RunReport};
use executor::{MapSecrets, Retention};
use executor_host::HostExecutor;
use frontend::{Diagnostic, FileSource, MapFiles, NoFiles};
use frontend_gha::load;
use ir::Graph;
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

pub fn diagnostics(text: &str) -> Vec<Diagnostic> {
    diagnostics_with(text, &NoFiles)
}

pub fn diagnostics_with(text: &str, files: &dyn FileSource) -> Vec<Diagnostic> {
    load(".github/workflows/test.yml", text, files)
        .diagnostics
        .into_vec()
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

fn driver_for(graph: Graph, dir: &std::path::Path) -> Driver {
    let executor: Arc<dyn executor::Executor> =
        Arc::new(HostExecutor::new(dir).with_retention(Retention::Never));
    let mut runners = steps::Registry::new();
    runners.register(steps::ProcessStep);
    runners.register(steps::NoopStep);
    Driver::new(
        graph,
        executor,
        runners,
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir).with_grace(Duration::from_secs(1)),
    )
}

/// Run on the host executor, with replay verified.
pub async fn run_host(graph: Graph, label: &str) -> RunReportPlus {
    let graph = with_params(graph);
    let dir = run_dir(label);
    let report = driver_for(graph.clone(), &dir).run().await;
    engine::verify_replay(graph, &report.state.log).expect("replay is byte-identical");
    let _ = std::fs::remove_dir_all(&dir);
    RunReportPlus::from(report)
}

/// Start a run, cancel it once `node` has started, and return the report.
pub async fn run_host_then_cancel(graph: Graph, label: &str, node: &str) -> (RunReportPlus, ()) {
    let graph = with_params(graph);
    let dir = run_dir(label);
    let driver = driver_for(graph, &dir);
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
