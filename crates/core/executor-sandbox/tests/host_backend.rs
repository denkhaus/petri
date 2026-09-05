//! The Host plugin: spawn, output, exit status, environment, and the
//! cancellation ladder, with no daemon.

use std::path::Path;
use std::time::Duration;

use executor::{AcquireContext, Executor, ProcessSpec, ScopeOutcome, ScopeSpec, Sig};
use executor_sandbox::HostExecutor;
use testkit::RunDir;
use tokio::time::timeout;

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
async fn dropping_a_process_closes_its_output_without_releasing_the_scope() {
    let dir = RunDir::new("host-process-drop");
    let executor = HostExecutor::new(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "echo ready; sleep 300"]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let ready = timeout(Duration::from_secs(10), lines.recv())
        .await
        .expect("the process started")
        .expect("the process wrote a line");
    assert_eq!(ready.line, "ready");

    drop(process);
    assert!(
        timeout(Duration::from_secs(1), lines.recv())
            .await
            .expect("the abandoned process's pumps were stopped")
            .is_none()
    );
    // The process owns its helpers independently of the reusable scope.
    let mut next = handle
        .exec()
        .spawn(ProcessSpec::new("true", &[]))
        .await
        .expect("the scope still accepts work");
    assert!(next.wait().await.expect("wait").is_success());
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[tokio::test]
async fn a_step_runs_and_reports_its_exit_code() {
    let dir = RunDir::new("host-exit-code");
    let executor = HostExecutor::new(dir.path());
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
    let dir = RunDir::new("host-scope-env");
    let executor = HostExecutor::new(dir.path());
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
    let dir = RunDir::new("host-sigterm");
    let executor = HostExecutor::new(dir.path());
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
    let dir = RunDir::new("host-workspace-io");
    let executor = HostExecutor::new(dir.path());
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
