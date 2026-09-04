//! The routing executor, at the executor level: routing, the scope-bound
//! one-shot runner in every execution mode, and the crash and cancel sweeps
//! that keep one-shot containers from leaking.

use std::path::Path;
use std::time::{Duration, Instant};

use executor::{
    AcquireContext, Executor as _, OneShotContainer, Retention, ScopeOutcome, ScopeSpec,
    ServiceSpec, Sig,
};
use executor_sandbox::{HostExecutor, RoutingExecutor, list_containers};
use ir::{RuntimeSpec, ScopeId};
use testkit::{RunDir, is_docker_ready, wait_for_file};
use tokio::time;

const IMAGE: &str = "alpine:3.20";

fn host_spec() -> ScopeSpec {
    ScopeSpec::new(ScopeId::new(0), "scope-0")
}

fn container_spec() -> ScopeSpec {
    host_spec().with_runtime(RuntimeSpec::container(IMAGE))
}

fn local(dir: &RunDir) -> RoutingExecutor {
    RoutingExecutor::local(dir.path().to_path_buf(), Retention::default())
}

/// Every log line of a one-shot handle, drained to completion.
async fn drain(handle: &mut Box<dyn executor::ProcessHandle>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(mut stream) = handle.lines() {
        while let Some(line) = stream.recv().await {
            lines.push(line.line);
        }
    }
    lines
}

/// The one-shot prefix a fresh router over the same run dir computes for a
/// scope, so a leak check reaches exactly this test's containers.
async fn one_shot_prefix(dir: &RunDir, instance: &str) -> String {
    local(dir)
        .one_shot_prefix_for(instance)
        .await
        .expect("the run id is recorded")
}

/// Services require a containerized job: a bare host process has no route to a
/// sidecar's network alias, so the host executor refuses and names the fix.
#[tokio::test]
async fn the_host_executor_refuses_services() {
    let dir = RunDir::new("host-refuses-services");
    let executor = HostExecutor::new(dir.path());
    let spec = host_spec().with_services(vec![ServiceSpec::new("redis", "redis:7")]);
    let error = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect_err("the host executor cannot realize services");
    assert!(error.to_string().contains("containerized job"), "{error}");
}

/// A host scope acquired through the router carries a runner, without touching
/// any daemon. Docker-free machines still run host scopes.
#[tokio::test]
async fn a_local_host_scope_is_bound_to_a_runner() {
    let dir = RunDir::new("local-host-runner");
    let executor = local(&dir);
    let handle = executor
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a runner rides the handle");
    assert_eq!(runner.workspace_path(), "/workspace");
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// Host scope: a one-shot container runs in the scope's world — its output is
/// captured, its exit code is the container's, and the scope workspace is
/// mounted where the runner says it is.
#[tokio::test]
async fn a_one_shot_container_runs_in_a_host_scope() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("local-one-shot");
    let executor = local(&dir);
    let handle = executor
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a runner rides the handle");

    let spec = OneShotContainer::registry(IMAGE).with_args(&[
        "sh",
        "-c",
        "echo one-shot ran; echo mark > /workspace/mark",
    ]);
    let mut process = runner.run(spec).await.expect("docker run");
    let lines = drain(&mut process).await;
    let status = process.wait().await.expect("wait");
    assert!(status.is_success(), "{status:?} {lines:?}");
    assert!(lines.iter().any(|l| l == "one-shot ran"), "{lines:?}");
    assert!(
        dir.workspace().join("mark").exists(),
        "the scope workspace is mounted at the runner's workspace path"
    );

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// Container scope: the handle carries a runner too, and its one-shots see the
/// same workspace as the job container.
#[tokio::test]
async fn a_pure_docker_scope_is_bound_to_a_runner() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-scope-runner");
    let executor = local(&dir);
    let handle = executor
        .acquire(&container_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a runner rides the handle");

    let spec = OneShotContainer::registry(IMAGE).with_args(&[
        "sh",
        "-c",
        "echo from-one-shot > /workspace/shared",
    ]);
    let mut process = runner.run(spec).await.expect("docker run");
    let _ = drain(&mut process).await;
    let status = process.wait().await.expect("wait");
    assert!(status.is_success(), "{status:?}");
    // The job container and the one-shot share the workspace bind mount.
    let shared = handle
        .exec()
        .read_file(Path::new("shared"))
        .await
        .expect("read through the job environment")
        .expect("the file the one-shot wrote");
    assert_eq!(String::from_utf8_lossy(&shared).trim(), "from-one-shot");

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// The cancel path: a signalled one-shot dies, `--rm` removes it, and nothing
/// of it is left under the scope's one-shot prefix.
#[tokio::test]
async fn a_signalled_one_shot_dies_and_leaves_nothing() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("local-one-shot-cancel");
    let executor = local(&dir);
    let handle = executor
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle.container_runner().expect("a runner");

    let spec = OneShotContainer::registry(IMAGE).with_args(&[
        "sh",
        "-c",
        "echo ready > /workspace/ready; sleep 300",
    ]);
    let mut process = runner.run(spec).await.expect("docker run");
    assert!(
        wait_for_file(&dir.workspace().join("ready"), Duration::from_secs(60)).await,
        "the one-shot never started"
    );
    process.signal(Sig::Term).await.expect("signal");
    let status = time::timeout(Duration::from_secs(30), process.wait())
        .await
        .expect("the signalled container ends")
        .expect("wait");
    assert!(!status.is_success(), "TERM ended it: {status:?}");

    let prefix = one_shot_prefix(&dir, "scope-0").await;
    assert!(
        list_containers(&prefix).await.is_empty(),
        "nothing is left under {prefix}"
    );
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// The crash path: a one-shot whose client died keeps running; the next acquire
/// of the same scope over the same run dir fences it away, and release sweeps
/// whatever a live run abandons.
#[tokio::test]
async fn crash_leftovers_are_fenced_by_acquire_and_swept_by_release() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("local-one-shot-crash");
    let prefix = one_shot_prefix(&dir, "scope-0").await;

    // "Crash": drop the handle without signalling. `kill_on_drop` ends the
    // `docker run` client, but the container keeps running — exactly what a
    // dead driver leaves behind.
    {
        let executor = local(&dir);
        let handle = executor
            .acquire(&host_spec(), &AcquireContext::bare())
            .await
            .expect("acquire");
        let runner = handle.container_runner().expect("a runner");
        let spec = OneShotContainer::registry(IMAGE).with_args(&[
            "sh",
            "-c",
            "echo ready > /workspace/ready; sleep 300",
        ]);
        let process = runner.run(spec).await.expect("docker run");
        assert!(
            wait_for_file(&dir.workspace().join("ready"), Duration::from_secs(60)).await,
            "the one-shot never started"
        );
        drop(process);
        drop(handle);
    }
    // Client death is not container death.
    time::sleep(Duration::from_millis(300)).await;
    assert!(
        !list_containers(&prefix).await.is_empty(),
        "the leftover container survives its client"
    );

    // A resuming process over the same run dir reaches the same names: the
    // fence removes the leftover before the scope is used again.
    let resumed = local(&dir);
    let handle = resumed
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("re-acquire");
    assert!(
        list_containers(&prefix).await.is_empty(),
        "acquire fenced the crashed one-shot away"
    );

    // And a live run's abandoned one-shot goes with the scope: release sweeps.
    let runner = handle.container_runner().expect("a runner");
    let spec = OneShotContainer::registry(IMAGE).with_args(&["sleep", "300"]);
    let _process = runner.run(spec).await.expect("docker run");
    let deadline = Instant::now() + Duration::from_secs(30);
    while list_containers(&prefix).await.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the abandoned one-shot never appeared"
        );
        time::sleep(Duration::from_millis(100)).await;
    }
    let report = resumed.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        list_containers(&prefix).await.is_empty(),
        "release swept the abandoned one-shot"
    );
}
