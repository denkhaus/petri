//! The sandbox executor over the in-process host provider: spawn, output,
//! exit status, environment, and the cancellation ladder, with no daemon.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use executor::{AcquireContext, Executor, ProcessSpec, ScopeOutcome, ScopeSpec, Sig};
use executor_sandbox::{BackendKind, SandboxExecutor};
use sandbox_driver::SandboxProvider;
use sandbox_driver_host::HostProvider;
use tokio::time::timeout;

#[allow(unreachable_pub, reason = "test-local helper module")]
mod tmp {
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs, process};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            let base = env::temp_dir().join(format!(
                "petri-sandbox-adapter-{}-{}",
                process::id(),
                nanos()
            ));
            fs::create_dir_all(&base).expect("create temp dir");
            Self(base)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    }
}

fn host_executor(run_dir: &Path) -> SandboxExecutor {
    let provider: Arc<dyn SandboxProvider> = Arc::new(HostProvider::new());
    SandboxExecutor::new(provider, BackendKind::Host, run_dir.to_path_buf())
}

fn scope() -> ScopeSpec {
    ScopeSpec::new(ir::ScopeId::new(1), "env-1").with_env(
        [(
            smol_str::SmolStr::new("SCOPE_VAR"),
            smol_str::SmolStr::new("scope-value"),
        )]
        .into_iter()
        .collect(),
    )
}

#[tokio::test]
async fn a_step_runs_and_reports_its_exit_code() {
    let dir = tmp::TempDir::new();
    let executor = host_executor(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "echo hello; exit 3"]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = Vec::new();
    while let Some(line) = lines.recv().await {
        seen.push(line.line);
    }
    let status = process.wait().await.expect("wait");
    assert_eq!(seen, vec!["hello".to_owned()]);
    assert_eq!(status.code, Some(3));
    executor.release(handle, ScopeOutcome::Failed).await;
}

#[tokio::test]
async fn scope_env_reaches_the_process() {
    let dir = tmp::TempDir::new();
    let executor = host_executor(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &[
            "-c",
            "printf '%s' \"$SCOPE_VAR\"",
        ]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = String::new();
    while let Some(line) = lines.recv().await {
        seen.push_str(&line.line);
    }
    process.wait().await.expect("wait");
    assert_eq!(seen, "scope-value");
    executor.release(handle, ScopeOutcome::Succeeded).await;
}

#[tokio::test]
async fn a_sigterm_ends_a_sleeping_step() {
    let dir = tmp::TempDir::new();
    let executor = host_executor(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "sleep 300"]))
        .await
        .expect("spawn");
    process.signal(Sig::Term).await.expect("signal");
    let status = timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("wait returned in time")
        .expect("wait");
    assert!(
        status.signal.is_some(),
        "expected a signalled status: {status:?}"
    );
    executor.release(handle, ScopeOutcome::Succeeded).await;
}

#[tokio::test]
async fn the_workspace_is_written_and_read() {
    let dir = tmp::TempDir::new();
    let executor = host_executor(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    env.write_file(Path::new("note.txt"), b"hi")
        .await
        .expect("write");
    let read = env.read_file(Path::new("note.txt")).await.expect("read");
    assert_eq!(read.as_deref(), Some(b"hi".as_slice()));
    let missing = env
        .read_file(Path::new("nope.txt"))
        .await
        .expect("read missing");
    assert_eq!(missing, None);
    executor.release(handle, ScopeOutcome::Succeeded).await;
}
