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

use std::sync::Arc;

use acceptance::runs::RUNNER_IMAGE_2404;
use github_actions::{ActionSourceCap, ActionTreeSource};
use support::*;

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

async fn run_artifact_flow(label: &str, container: Option<&str>) {
    let source = corpus_action_source();
    let graph = lower_with_actions(&artifact_workflow(container), &source);
    let report = run_host_with(graph, label, |rt| {
        let trees: Arc<dyn ActionTreeSource> = source;
        with_object_service(rt.capability(ActionSourceCap(trees)), None)
    })
    .await;

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
}

/// Host jobs: upload in one job, download in the next, through loopback.
#[tokio::test(flavor = "multi_thread")]
async fn artifacts_flow_across_host_jobs() {
    if !tool_ready("node") {
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

/// The runtime tier's whole story in one workflow, the plan's done-condition:
/// checkout + build + upload in one job, download + verify in the next, to
/// `Success` — the checkout local, the artifacts local, the actions from the
/// warm cache. No network beyond what a container image pull would need.
#[tokio::test(flavor = "multi_thread")]
async fn checkout_build_and_artifacts_run_to_success() {
    if !tool_ready("node") {
        return;
    }
    let repo = std::env::temp_dir()
        .join("petri-full-flow")
        .join(format!("repo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("create the fixture");
    std::fs::write(repo.join("input.txt"), "source-of-truth\n").expect("write");
    commit_fixture(&repo);

    let text = format!(
        "on: push\n\
         jobs:\n\
         \x20 build:\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   steps:\n\
         \x20     - uses: actions/checkout@v4\n\
         \x20     - run: mkdir -p dist && tr a-z A-Z < input.txt > dist/built.txt\n\
         \x20     - uses: {UPLOAD}\n\
         \x20       with:\n\
         \x20         name: built\n\
         \x20         path: dist/built.txt\n\
         \x20 verify:\n\
         \x20   needs: build\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   steps:\n\
         \x20     - uses: {DOWNLOAD}\n\
         \x20       with:\n\
         \x20         name: built\n\
         \x20         path: got\n\
         \x20     - run: cat got/built.txt\n"
    );
    let source = corpus_action_source();
    let mut graph = lower_with_actions(&text, &source);
    graph.params.insert(
        "petri".into(),
        serde_json::json!({ "repo": repo.display().to_string() }),
    );
    let report = run_host_with(graph, "full-flow", |rt| {
        let trees: Arc<dyn ActionTreeSource> = source;
        with_object_service(rt.capability(ActionSourceCap(trees)), None)
    })
    .await;
    assert_eq!(
        report.status,
        runtime::ir::RunStatus::Success,
        "statuses: {:?}\nlog: {:?}",
        report
            .state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag()))
            .collect::<Vec<_>>(),
        log_lines(&report)
    );
    assert!(
        log_lines(&report).iter().any(|l| l == "SOURCE-OF-TRUTH"),
        "the build's output round-tripped: {:?}",
        log_lines(&report)
    );
    let _ = std::fs::remove_dir_all(&repo);
}
