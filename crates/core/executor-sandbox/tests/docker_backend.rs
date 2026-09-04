//! The sandbox executor over the Docker provider: a container scope with the
//! workspace bind-mounted, exec exit codes, and the host-visible workspace.
//! Skips (passes trivially) when no Docker daemon is reachable.

use std::fs;
use std::path::Path;
use std::sync::Arc;

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
