//! Cross-job artifact flow, end to end: the real `actions/upload-artifact` and
//! `actions/download-artifact` (pinned) against the per-run ObjectService —
//! the runtime tier's one hard service blocker, proven in a host job and in
//! containerized jobs.
//!
//! The service rides the same `Runtime::run_services` seam the distribution
//! wires: started beside the run dir, capability to the steps, torn down with
//! the driver. Fetching the two pinned actions needs the network once; the
//! git cache under the corpus keeps every later run offline.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acceptance::runs::RUNNER_IMAGE_2404;
use github_actions::{
    ActionSource, ActionSourceCap, ActionTreeSource, GitActionSource, ResultsServiceCap,
};
use github_objects::ObjectService;
use runtime::executor::Retention;
use runtime::frontend::NoFiles;
use runtime::ir::Graph;
use runtime::{RunOptions, Runtime};
use support::*;

/// The corpus's shared action cache: the acceptance battery and the sweep pull
/// the same pinned trees once.
fn action_source() -> Arc<GitActionSource> {
    let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus/.actions-cache");
    Arc::new(GitActionSource::new(cache))
}

/// Pinned at v4 of each — the era whose toolkit speaks the results service.
const UPLOAD: &str = "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02";
const DOWNLOAD: &str = "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093";

fn artifact_workflow(container: Option<&str>) -> String {
    let container = container.map_or(String::new(), |image| format!("    container: {image}\n"));
    format!(
        "on: push\n\
         jobs:\n\
         \x20 build:\n\
         \x20   runs-on: ubuntu-latest\n\
         {container}\
         \x20   steps:\n\
         \x20     - name: make\n\
         \x20       run: mkdir -p dist && echo payload-42 > dist/note.txt\n\
         \x20     - name: upload\n\
         \x20       uses: {UPLOAD}\n\
         \x20       with:\n\
         \x20         name: dist\n\
         \x20         path: dist/note.txt\n\
         \x20 fetch:\n\
         \x20   needs: build\n\
         \x20   runs-on: ubuntu-latest\n\
         {container}\
         \x20   steps:\n\
         \x20     - name: download\n\
         \x20       uses: {DOWNLOAD}\n\
         \x20       with:\n\
         \x20         name: dist\n\
         \x20         path: got\n\
         \x20     - name: read\n\
         \x20       run: cat got/note.txt\n"
    )
}

fn lower_with_actions(text: &str, source: &Arc<GitActionSource>) -> Graph {
    let actions: Arc<dyn ActionSource> = Arc::clone(source) as _;
    let lowered = frontend_gha::load_with(
        ".github/workflows/test.yml",
        text,
        &NoFiles,
        Some(actions.as_ref()),
    );
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("the workflow lowers")
}

/// The distribution's per-run wiring, assembled here because a component's
/// tests may not depend on the distribution: the ObjectService starts beside
/// the run dir and its capability reaches the steps.
fn artifact_runtime(dir: &Path, source: Arc<GitActionSource>) -> Runtime {
    let trees: Arc<dyn ActionTreeSource> = source;
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    Runtime::standard()
        .options(options)
        .step(github_actions::RunStep)
        .step(github_actions::ActionStep)
        .step(github_actions::DockerActionStep)
        .capability(ActionSourceCap(trees))
        .run_services(
            |run_dir, caps| match ObjectService::start(run_dir.join("artifacts")) {
                Ok(service) => {
                    let cap = ResultsServiceCap {
                        port: service.port(),
                        token: service.token().into(),
                    };
                    (caps.provide(cap), Some(Box::new(service) as _))
                }
                Err(error) => {
                    eprintln!("warning: no results service: {error}");
                    (caps, None)
                }
            },
        )
}

async fn run_artifact_flow(label: &str, container: Option<&str>) {
    let source = action_source();
    let graph = with_params(lower_with_actions(&artifact_workflow(container), &source));
    let dir = std::env::temp_dir()
        .join("petri-artifacts-e2e")
        .join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let report = artifact_runtime(&dir, source)
        .run(graph)
        .await
        .expect("replay is byte-identical");
    let report = RunReportPlus::from(report);

    let statuses: Vec<(String, String)> = report
        .state
        .history()
        .iter()
        .map(|r| (r.name.to_string(), r.outcome.status.tag().to_string()))
        .collect();
    if statuses.iter().any(|(_, s)| s == "failure") {
        for line in log_lines(&report) {
            eprintln!("  | {line}");
        }
    }
    assert_eq!(
        status_of(&report, "build/step-2").as_deref(),
        Some("success"),
        "upload succeeded: {statuses:?}",
    );
    assert_eq!(
        status_of(&report, "fetch/step-1").as_deref(),
        Some("success"),
        "download succeeded: {statuses:?}",
    );
    // The downloaded content is the uploaded content, read back in the second job.
    assert!(
        log_lines(&report).iter().any(|l| l == "payload-42"),
        "the artifact round-tripped",
    );
    assert_eq!(report.status, runtime::ir::RunStatus::Success);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Host jobs: upload in one job, download in the next, through loopback.
#[tokio::test(flavor = "multi_thread")]
async fn artifacts_flow_across_host_jobs() {
    if !node_ready() {
        return;
    }
    run_artifact_flow("host", None).await;
}

/// Containerized jobs: the same flow, the service reached through the
/// executor's guaranteed `host.docker.internal` alias.
#[tokio::test(flavor = "multi_thread")]
async fn artifacts_flow_across_containerized_jobs() {
    if !testkit::docker_ready().await {
        return;
    }
    run_artifact_flow("boxed", Some(RUNNER_IMAGE_2404)).await;
}

/// JavaScript actions need `node` on `PATH` for the host case; without it the
/// host half skips exactly as the hashFiles test does.
fn node_ready() -> bool {
    let found = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipping: no `node` on PATH");
    }
    found
}
