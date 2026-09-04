//! The sandbox executor over the Docker provider: a container scope with the
//! workspace bind-mounted, exec exit codes, and the host-visible workspace.
//! Skips (passes trivially) when no Docker daemon is reachable.

use std::path::Path;
use std::sync::Arc;
use std::{fs, mem};

use executor::{AcquireContext, Executor, ProcessSpec, ScopeOutcome, ScopeSpec};
use executor_sandbox::{BackendKind, SandboxExecutor};
use sandbox_driver::SandboxProvider;
use sandbox_driver_docker::DockerProvider;

const TEST_IMAGE: &str = "buildpack-deps:noble";

mod tmp {
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs, process};

    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let base = env::temp_dir().join(format!(
                "petri-sandbox-docker-{}-{}",
                process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            fs::create_dir_all(&base).expect("create temp dir");
            Self(base)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

async fn docker_executor(run_dir: &Path) -> Option<SandboxExecutor> {
    let provider = DockerProvider::connect().await.ok()?;
    let provider: Arc<dyn SandboxProvider> = Arc::new(provider);
    Some(SandboxExecutor::new(
        provider,
        BackendKind::Docker,
        run_dir.to_path_buf(),
    ))
}

fn container_scope() -> ScopeSpec {
    ScopeSpec::new(ir::ScopeId::new(1), "env-1")
        .with_runtime(ir::RuntimeSpec::container(TEST_IMAGE))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_step_runs_and_the_workspace_is_a_host_bind_mount() {
    let dir = tmp::TempDir::new();
    let Some(executor) = docker_executor(dir.path()).await else {
        return;
    };
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

    let host_file = dir
        .path()
        .join("scopes")
        .join("env-1")
        .join("work")
        .join("out.txt");
    let on_host = fs::read_to_string(&host_file).expect("read host bind mount");
    assert_eq!(on_host.trim(), "written");

    executor.release(handle, ScopeOutcome::Failed).await;
    // Release deletes the container and, on failure retention, keeps the
    // host workspace.
    assert!(host_file.exists(), "failed workspace kept for debugging");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reacquire_fences_the_crashed_predecessor() {
    use sandbox_driver::SandboxFilter;

    let dir = tmp::TempDir::new();
    let Some(first) = docker_executor(dir.path()).await else {
        return;
    };
    let handle = first
        .acquire(&container_scope(), &AcquireContext::bare())
        .await
        .expect("first acquire");

    // The provider sees exactly one sandbox for this environment.
    let provider = DockerProvider::connect().await.expect("docker");
    let mut filter = SandboxFilter::default();
    filter
        .labels
        .insert("petri.environment".to_owned(), env_label(dir.path()));
    let before = provider.list(&filter).await.expect("list");
    assert_eq!(before.len(), 1, "one live sandbox");
    let crashed_id = before[0].id.clone();

    // Simulate a crash: drop the handle without releasing, so the container
    // survives. A second executor over the same run dir must fence it.
    mem::forget(handle);
    let second = docker_executor(dir.path()).await.expect("second executor");
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

/// The environment label the fence keys on, recomputed from the run dir's
/// recorded run id.
fn env_label(run_dir: &Path) -> String {
    let run_id = fs::read_to_string(run_dir.join("sandbox-run-id"))
        .expect("run id recorded")
        .trim()
        .to_owned();
    // The container scope uses environment id "env-1".
    format!("{run_id}/env-1")
}
