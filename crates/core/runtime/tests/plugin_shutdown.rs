//! Finishing one run closes its plugins without ending another run's provider.

use std::fs;
use std::path::Path;

use executor::{AcquireContext, Executor as _, Retention, ScopeOutcome, ScopeSpec};
use ir::{RunStatus, ScopeId};
use runtime::{RunOptions, Runtime};
use testkit::RunDir;

#[tokio::test]
async fn run_finish_closes_its_plugin_and_preserves_other_runs_and_retained_files() {
    let first_dir = RunDir::new("plugin-finish-first");
    let second_dir = RunDir::new("plugin-finish-second");
    let mut options = RunOptions::new(first_dir.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard().options(options);
    let first = runtime.prepare_run(first_dir.path());
    let second = runtime.prepare_run(second_dir.path());
    let first_router = first.sandbox_router().expect("standard router").clone();
    let second_router = second.sandbox_router().expect("standard router").clone();
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
    let first_env = first_router
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("first host scope");
    let second_env = second_router
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("second host scope");
    let first_exec = first_env.exec();
    first_exec
        .write_file(Path::new("retained"), b"kept")
        .await
        .expect("retained data");
    assert!(
        first_router
            .release(first_env, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert_eq!(
        first_exec
            .read_file(Path::new("retained"))
            .await
            .expect("live plugin"),
        Some(b"kept".to_vec())
    );

    first.finish_with_status(RunStatus::Success).await;

    // Keep the old router and environment references alive: drop alone cannot
    // close this transport. The run must explicitly await plugin shutdown.
    assert!(first_exec.read_file(Path::new("retained")).await.is_err());
    assert_eq!(
        fs::read(first_dir.workspace().join("retained")).expect("retained workspace"),
        b"kept"
    );
    assert!(first_dir.path().join("host-registry").is_dir());
    second_env
        .exec()
        .write_file(Path::new("still-live"), b"yes")
        .await
        .expect("other run remains live");
    assert!(
        second_router
            .release(second_env, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    second.finish_with_status(RunStatus::Success).await;
}
