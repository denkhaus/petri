//! Sidecar services at the executor level: realized with the scope, healthy
//! before acquire returns, reachable from the scope's world, torn down with
//! release — and a failed service fails the acquire without leaking.

use executor::{AcquireContext, Executor, OneShotContainer, ScopeOutcome, ScopeSpec, ServiceSpec};
use executor_docker::{DockerExecutor, list_containers};
use ir::{RuntimeSpec, ScopeId};
use runtime::LocalExecutor;
use smol_str::SmolStr;
use testkit::{RunDir, docker_available};

async fn docker_ready() -> bool {
    if docker_available().await {
        return true;
    }
    if std::env::var("PETRI_REQUIRE_DOCKER").is_ok_and(|v| !v.is_empty()) {
        panic!("PETRI_REQUIRE_DOCKER is set, but no Docker daemon is reachable");
    }
    eprintln!("skipping: no Docker daemon reachable");
    false
}

const REDIS: &str = "redis:7-alpine";

fn redis_service(published: Option<&str>) -> ServiceSpec {
    let mut service = ServiceSpec::new("redis", REDIS);
    service.options = [
        "--health-cmd",
        "redis-cli ping",
        "--health-interval",
        "1s",
        "--health-timeout",
        "3s",
        "--health-retries",
        "30",
    ]
    .iter()
    .map(|o| SmolStr::new(*o))
    .collect();
    if let Some(ports) = published {
        service.ports = vec![SmolStr::new(ports)];
    }
    service
}

/// Host scope: the service comes up healthy before acquire returns, its port
/// is published to the host, the scope's one-shot containers share its
/// network, and release removes containers and network alike.
#[tokio::test]
async fn a_host_scope_realizes_and_tears_down_services() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("services-host");
    let executor = LocalExecutor::new(dir.path());
    let spec =
        ScopeSpec::new(ScopeId::new(0), "scope-0").with_services(vec![redis_service(Some(
            "29811:6379",
        ))]);
    let handle = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect("acquire realizes the service");

    // Healthy before acquire returned: the published port answers now.
    std::net::TcpStream::connect("127.0.0.1:29811").expect("the published port answers");

    // One-shots run on the scope's network and resolve the service by name.
    let runner = handle.container_runner().expect("a runner");
    let one_shot = OneShotContainer::registry("alpine:3.20").with_args(&[
        "sh",
        "-c",
        "nslookup redis",
    ]);
    let mut process = runner.run(one_shot).await.expect("docker run");
    let status = process.wait().await.expect("wait");
    assert!(status.success(), "the service name resolves: {status:?}");

    let base = container_base(dir.path(), "scope-0").await;
    assert_eq!(list_containers(&format!("{base}-svc-")).await.len(), 1);
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        list_containers(&format!("{base}-svc-")).await.is_empty(),
        "release removed the service container"
    );
    assert!(
        network_gone(&base).await,
        "release removed the scope network"
    );
}

/// Container scope: the job container joins the service network at creation
/// and reaches the service by name; teardown removes the whole world.
#[tokio::test]
async fn a_container_scope_reaches_its_service_by_name() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("services-container");
    let executor = DockerExecutor::new(dir.path());
    let spec = ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_runtime(RuntimeSpec::container("alpine:3.20"))
        .with_services(vec![redis_service(None)]);
    let handle = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect("acquire realizes the service");

    let mut process = handle
        .exec()
        .spawn(executor::ProcessSpec::new("sh", &["-c", "nslookup redis"]))
        .await
        .expect("spawn in the job container");
    let status = process.wait().await.expect("wait");
    assert!(status.success(), "the job resolves the service: {status:?}");

    let base = container_base(dir.path(), "scope-0").await;
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(list_containers(&base).await.is_empty(), "nothing is left");
    assert!(network_gone(&base).await, "the scope network is gone");
}

/// A service that exits before it is ready fails the acquire — routably, with
/// the service named — and the partial world is torn down before the error
/// returns.
#[tokio::test]
async fn a_dead_service_fails_the_acquire_and_leaks_nothing() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("services-dead");
    let executor = LocalExecutor::new(dir.path());
    // Plain alpine has nothing long-running: the container exits at once.
    let spec = ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_services(vec![ServiceSpec::new("flaky", "alpine:3.20")]);
    let error = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect_err("a dead service fails the scope");
    assert!(error.to_string().contains("flaky"), "{error}");

    let base = container_base(dir.path(), "scope-0").await;
    assert!(
        list_containers(&format!("{base}-svc-")).await.is_empty(),
        "the failed acquire left no service container"
    );
    assert!(network_gone(&base).await, "the failed acquire left no network");
}

/// The scope's base container name, computed the way the executors compute it.
async fn container_base(run_dir: &std::path::Path, instance: &str) -> String {
    let prefix = DockerExecutor::new(run_dir)
        .container_prefix()
        .await
        .expect("the run id is recorded");
    format!("{prefix}{instance}")
}

async fn network_gone(base: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["network", "inspect", base])
        .output()
        .await
        .map(|out| !out.status.success())
        .unwrap_or(true)
}
