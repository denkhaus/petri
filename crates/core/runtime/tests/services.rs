//! Sidecar services at the executor level: realized with a container scope,
//! healthy before acquire returns, reachable from the job by name, torn down
//! with release — a failed service fails the acquire without leaking — and
//! refused for a bare host process, which has no route to a service's alias.

use executor::{AcquireContext, Executor as _, Retention, ScopeOutcome, ScopeSpec, ServiceSpec};
use executor_sandbox::RoutingExecutor;
use ir::{RuntimeSpec, ScopeId};
use smol_str::SmolStr;
use testkit::{RunDir, is_docker_ready, list_containers};
use tokio::process::Command;

const REDIS: &str = "redis:7-alpine";
// The trailing dot prevents a host-provided DNS search suffix from hiding
// Docker's network alias.
const RESOLVE_REDIS: &str = "nslookup redis.";

fn redis_service() -> ServiceSpec {
    let mut service = ServiceSpec::new("redis", REDIS);
    service.options.health = Some(ir::HealthCheck {
        cmd: Some(SmolStr::new("redis-cli ping")),
        interval_ms: Some(1_000),
        timeout_ms: Some(3_000),
        retries: Some(30),
        ..ir::HealthCheck::default()
    });
    service
}

fn local(dir: &RunDir) -> RoutingExecutor {
    RoutingExecutor::local(dir.path(), Retention::default())
}

/// A host scope with services fails at acquire, routably, naming the fix, and
/// realizes nothing: the sidecar model needs the job container's network.
#[tokio::test]
async fn a_host_scope_refuses_services() {
    let dir = RunDir::new("services-host-refused");
    let executor = local(&dir);
    let spec = ScopeSpec::new(ScopeId::new(0), "scope-0").with_services(vec![redis_service()]);
    let error = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect_err("services need a containerized job");
    assert!(error.to_string().contains("containerized job"), "{error}");
}

/// Container scope: the job container joins the service network at creation,
/// the service is healthy before acquire returns, the job reaches it by name,
/// and teardown removes the whole world.
#[tokio::test]
async fn a_container_scope_reaches_its_service_by_name() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("services-container");
    let executor = local(&dir);
    let spec = ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_runtime(RuntimeSpec::container("alpine:3.20"))
        .with_services(vec![redis_service()]);
    let handle = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect("acquire realizes the service");

    let mut process = handle
        .exec()
        .spawn(executor::ProcessSpec::new("sh", &["-c", RESOLVE_REDIS]))
        .await
        .expect("spawn in the job container");
    let status = process.wait().await.expect("wait");
    assert!(
        status.is_success(),
        "the job resolves the service: {status:?}"
    );

    let base = container_base(&dir, 0).await;
    let network = format!("{base}-net");
    assert_eq!(
        list_containers(&format!("{network}-")).await.len(),
        1,
        "one sidecar under the scope network"
    );
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(list_containers(&base).await.is_empty(), "nothing is left");
    assert!(is_network_gone(&network).await, "the scope network is gone");
}

/// A service that exits before it is ready fails the acquire — routably, with
/// the service named — and the partial world is torn down before the error
/// returns.
#[tokio::test]
async fn a_dead_service_fails_the_acquire_and_leaks_nothing() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("services-dead");
    let executor = local(&dir);
    // Plain alpine has nothing long-running: the container exits at once.
    // The health check is what makes that a failure: it says the service
    // must come up, so the provider waits and notices. Without one a sidecar
    // may exit — a migration job does — and the acquire would succeed.
    let mut flaky = ServiceSpec::new("flaky", "alpine:3.20");
    flaky.options.health = Some(ir::HealthCheck {
        cmd: Some(SmolStr::new("true")),
        interval_ms: Some(1_000),
        ..ir::HealthCheck::default()
    });
    let spec = ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_runtime(RuntimeSpec::container("alpine:3.20"))
        .with_services(vec![flaky]);
    let error = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect_err("a dead service fails the scope");
    assert!(error.to_string().contains("flaky"), "{error}");

    let base = container_base(&dir, 0).await;
    let network = format!("{base}-net");
    assert!(
        list_containers(&base).await.is_empty(),
        "the failed acquire left no container"
    );
    assert!(
        is_network_gone(&network).await,
        "the failed acquire left no network"
    );
}

/// The scope's sandbox name, computed the way the executor computes it: the
/// run's container prefix plus the lease, which a bare executor keys by the
/// scope id.
async fn container_base(dir: &RunDir, scope: u64) -> String {
    let prefix = local(dir)
        .container_prefix()
        .await
        .expect("the run id is recorded");
    format!("{prefix}l{scope}")
}

async fn is_network_gone(network: &str) -> bool {
    Command::new("docker")
        .args(["network", "inspect", network])
        .output()
        .await
        .map_or(true, |out| !out.status.success())
}
