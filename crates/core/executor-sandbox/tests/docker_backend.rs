//! The sandbox executor over the Docker provider: a container scope with the
//! workspace bind-mounted, exec exit codes, and the host-visible workspace.
//! Skips when no Docker daemon is reachable, unless `PETRI_REQUIRE_DOCKER`
//! says the daemon must be there.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, mem};

use executor::{AcquireContext, Executor, ProcessSpec, ScopeOutcome, ScopeSpec};
use executor_sandbox::{ENVIRONMENT_LABEL, RUN_ID_FILE, SandboxExecutor};
use sandbox_driver::{SandboxFilter, SandboxProvider};
use sandbox_driver_docker::DockerProvider;
use testkit::{RunDir, is_docker_ready};

const TEST_IMAGE: &str = "buildpack-deps:noble";
/// The environment id every scope in this battery uses.
const INSTANCE: &str = "env-1";

async fn docker_executor(run_dir: &Path) -> SandboxExecutor {
    let provider = DockerProvider::connect().await.expect("docker daemon");
    let provider: Arc<dyn SandboxProvider> = Arc::new(provider);
    SandboxExecutor::new(provider, run_dir)
}

fn container_scope() -> ScopeSpec {
    ScopeSpec::new(ir::ScopeId::new(1), INSTANCE)
        .with_runtime(ir::RuntimeSpec::container(TEST_IMAGE))
}

/// The scope's workspace on the host: the bind-mount source.
fn host_workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes").join(INSTANCE).join("work")
}

/// The run id recorded under the run dir.
fn run_id(dir: &RunDir) -> String {
    fs::read_to_string(dir.path().join(RUN_ID_FILE))
        .expect("run id recorded")
        .trim()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_step_runs_and_the_workspace_is_a_host_bind_mount() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-step");
    let executor = docker_executor(dir.path()).await;
    let handle = executor
        .acquire(&container_scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    assert_eq!(env.workspace_path(), "/workspace");

    // A step writes into the container workspace; the same bytes appear on
    // the host bind mount.
    let mut process = env
        .spawn(ProcessSpec::new("bash", &[
            "-c",
            "echo written > /workspace/out.txt; echo done; exit 5",
        ]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = Vec::new();
    while let Some(line) = lines.recv().await {
        seen.push(line.line);
    }
    let status = process.wait().await.expect("wait");
    assert_eq!(seen, vec!["done".to_owned()]);
    assert_eq!(status.code, Some(5));

    let host_file = host_workspace(&dir).join("out.txt");
    let on_host = fs::read_to_string(&host_file).expect("read host bind mount");
    assert_eq!(on_host.trim(), "written");

    executor.release(handle, ScopeOutcome::Failed).await;
    // Release deletes the container and, on failure retention, keeps the
    // host workspace.
    assert!(host_file.exists(), "failed workspace kept for debugging");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reacquire_fences_the_crashed_predecessor() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-fence");
    let first = docker_executor(dir.path()).await;
    let handle = first
        .acquire(&container_scope(), &AcquireContext::bare())
        .await
        .expect("first acquire");

    // The provider sees exactly one sandbox for this environment.
    let provider = DockerProvider::connect().await.expect("docker");
    let mut filter = SandboxFilter::default();
    filter.labels.insert(
        ENVIRONMENT_LABEL.to_owned(),
        format!("{}/{INSTANCE}", run_id(&dir)),
    );
    let before = provider.list(&filter).await.expect("list");
    assert_eq!(before.len(), 1, "one live sandbox");
    let crashed_id = before[0].id.clone();

    // Simulate a crash: drop the handle without releasing, so the container
    // survives. A second executor over the same run dir must fence it.
    mem::forget(handle);
    let second = docker_executor(dir.path()).await;
    let handle2 = second
        .acquire(&container_scope(), &AcquireContext::bare())
        .await
        .expect("second acquire");

    let after = provider.list(&filter).await.expect("list again");
    assert_eq!(after.len(), 1, "still one sandbox after the fence");
    assert_ne!(
        after[0].id, crashed_id,
        "the crashed container was replaced"
    );

    second.release(handle2, ScopeOutcome::Succeeded).await;
    let cleaned = provider.list(&filter).await.expect("list after release");
    assert!(cleaned.is_empty(), "release removed the container");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_scope_realizes_and_sweeps_its_service() {
    use std::process::Command;

    use executor::ServiceSpec;

    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-service");
    let executor = docker_executor(dir.path()).await;
    // A declared service becomes a sidecar on a per-sandbox network; the
    // sidecar carries an env var the workload could read by alias. (The
    // image is not a daemon, so this checks the realize/sweep lifecycle, not
    // DNS liveness — sandbox-driver's own conformance covers a live service.)
    let mut scope = container_scope();
    let mut service = ServiceSpec::new("svc", TEST_IMAGE);
    service.env.insert("SVC_TOKEN".into(), "value".into());
    scope.services = vec![service];

    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("acquire with a service");

    // The sidecar network is named after this run's job container, so the
    // check is specific and immune to any leftover networks on the daemon.
    let prefix = executor.container_prefix().await.expect("run id");
    let network = format!("{prefix}{INSTANCE}-net");
    let exists = |name: &str| {
        let out = Command::new("docker")
            .args(["network", "ls", "--format", "{{.Name}}"])
            .output()
            .expect("docker network ls")
            .stdout;
        String::from_utf8(out)
            .expect("utf8")
            .lines()
            .any(|line| line == name)
    };
    assert!(
        exists(&network),
        "the sidecar network is present during the scope"
    );

    executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(!exists(&network), "release swept the sidecar network");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_action_container_shares_the_workspace() {
    use executor::OneShotContainer;

    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-one-shot");
    let executor = docker_executor(dir.path()).await;
    let handle = executor
        .acquire(&container_scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a container scope binds a one-shot runner");
    assert_eq!(runner.workspace_path(), "/workspace");

    // A one-shot action container writes into the shared workspace and exits
    // with its own status.
    let spec = OneShotContainer::registry(TEST_IMAGE)
        .with_entrypoint("bash")
        .with_args(&["-c", "echo from-action > /workspace/action.txt; exit 4"]);
    let mut process = runner.run(spec).await.expect("run one-shot");
    let status = process.wait().await.expect("wait");
    assert_eq!(status.code, Some(4));

    let host_file = host_workspace(&dir).join("action.txt");
    assert_eq!(
        fs::read_to_string(&host_file).expect("host file").trim(),
        "from-action"
    );
    executor.release(handle, ScopeOutcome::Succeeded).await;
}
