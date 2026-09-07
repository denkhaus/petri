//! Cancel repro for the dind wedge: a step drives a real `docker build`
//! against the *inner* daemon of a privileged dind runner, and the run is
//! cancelled mid-build. Measures how long the run takes to come down and
//! that nothing is left behind. Skips when no daemon is reachable or the
//! dind runner image is not present locally (it is ~1.5 GB).

mod support;

use std::time::{Duration, Instant};

use driver::RunConfig;
use executor::Retention;
use ir::{GraphBuilder, RunStatus, RuntimeSpec, RuntimeTarget, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;
use testkit::is_docker_ready;
use tokio::process::Command;
use tokio::time;

const DIND_IMAGE: &str = "ghcr.io/lithoscomputer/ubuntu-24.04:dind-df708f910111";

async fn dind_image_local() -> bool {
    Command::new("docker")
        .args(["image", "inspect", DIND_IMAGE])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

fn dind_graph(script: &str) -> ir::Graph {
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container(DIND_IMAGE);
    if let RuntimeTarget::Container { options, .. } = &mut scope.runtime.target {
        options.privileged = true;
    }
    let scope = b.add_scope(scope);
    b.add_node(
        "build",
        scope,
        StepRef::new(
            PROCESS_KIND,
            script_with(script, &json!({ "shell": "bash" })),
        ),
    );
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// Cancel arrives while `docker build` is executing a long RUN inside the
/// inner daemon. The run must come down promptly.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "the skip notice and timing readout belong to the test runner's output"
)]
async fn cancel_mid_inner_build_comes_down_promptly() {
    if !is_docker_ready().await || !dind_image_local().await {
        eprintln!("skipping: no daemon or no local dind image");
        return;
    }
    let dir = RunDir::new("dind-cancel");
    let graph = dind_graph(
        r"
start-docker > start-docker.log 2>&1
mkdir -p b
printf 'FROM alpine:3.20\nRUN sleep 600\n' > b/Dockerfile
echo ready > ready
docker build --progress=plain b 2>&1 | tee build.log
",
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let (driver, prefix) = docker_driver_named(graph, &dir, config).await;
    let sandbox = sandbox_name(dir.path(), 0);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_container_file(&sandbox, "/workspace/ready", Duration::from_secs(120)).await,
        "the step never started inside the container"
    );
    // The inner build's RUN is executing once the plain-progress log names it.
    let read = |path: &'static str| {
        let sandbox = sandbox.clone();
        async move {
            container_read(&sandbox, path)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        }
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if read("/workspace/build.log").await.contains("RUN sleep 600") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the inner build never reached its RUN; start-docker.log: {}\nbuild.log: {}",
            read("/workspace/start-docker.log").await,
            read("/workspace/build.log").await
        );
        time::sleep(Duration::from_millis(250)).await;
    }
    // Give the RUN a moment to actually be executing, not just named.
    time::sleep(Duration::from_secs(2)).await;

    handle.cancel(ir::CancelScopeId::ROOT).await;
    let cancelled_at = Instant::now();
    let report = time::timeout(Duration::from_secs(90), run)
        .await
        .expect("cancel did not bring the run down within the sweep's 90s")
        .expect("the run finished");
    let elapsed = cancelled_at.elapsed();
    eprintln!("cancel-to-down: {elapsed:?}");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "build").as_deref(), Some("cancelled"));
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "cancel took too long to bring the run down: {elapsed:?}"
    );
}
