//! The `scope_acquired` point: a host is handed a scope's environment after
//! the executor acquired it and before the first attempt runs in it, so it
//! can prepare the workspace through the environment, on this machine or in
//! a remote sandbox alike; a host that refuses fails the scope's firings.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use petri::driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, ScopeAcquired, ScopeAcquiredError,
};
use petri::executor::Retention;
use petri::ir::{GraphBuilder, RunStatus, ScopeId};
use petri::{RunOptions, Runtime};
use testkit::{RunDir, add_script, status_of};

/// A host that seeds every workspace it is handed with a file and keeps the
/// order of its callbacks, or refuses every environment.
#[derive(Default)]
struct SeedingHost {
    refuse:     bool,
    calls:      Mutex<Vec<String>>,
    workspaces: Mutex<Vec<String>>,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
impl ExecutionHooks for SeedingHost {
    async fn before_attempt(
        &self,
        _context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        lock(&self.calls).push(format!("before_attempt:{}", request.view.node_name()));
        AttemptDecision::admit()
    }

    async fn scope_acquired(
        &self,
        _context: &HookContext,
        acquired: ScopeAcquired,
    ) -> Result<(), ScopeAcquiredError> {
        lock(&self.calls).push(format!("scope_acquired:{}", acquired.scope.raw()));
        lock(&self.workspaces).push(acquired.workspace.as_str().to_owned());
        if self.refuse {
            return Err(ScopeAcquiredError::new("the host refused the workspace"));
        }
        acquired
            .env
            .write_file(Path::new("seed.txt"), b"seeded\n")
            .await
            .map_err(|error| ScopeAcquiredError::new(error.to_string()))?;
        Ok(())
    }
}

fn runtime(dir: &RunDir, host: &Arc<SeedingHost>) -> Runtime {
    // Keep the workspace after a success, so the test can read it.
    let mut options = RunOptions::new(dir.path());
    options.retention = Retention::Always;
    petri::runtime()
        .hooks(Arc::clone(host) as Arc<dyn ExecutionHooks>)
        .options(options)
}

#[tokio::test]
async fn the_host_prepares_the_workspace_before_the_first_attempt_runs_in_it() {
    let dir = RunDir::new("scope-acquired-seed");
    let host = Arc::new(SeedingHost::default());
    let mut b = GraphBuilder::new();
    let one = add_script(&mut b, "one", ScopeId::new(0), "cat seed.txt");
    let two = add_script(&mut b, "two", ScopeId::new(0), "test -f seed.txt");
    b.link(one, two);
    let report = runtime(&dir, &host)
        .run(b.build())
        .await
        .expect("the run replays");

    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(status_of(&report, "one").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "two").as_deref(), Some("success"));
    assert_eq!(
        fs::read_to_string(dir.workspace().join("seed.txt")).expect("the seed file"),
        "seeded\n",
        "the file the host wrote through the environment is in the scope's workspace"
    );
    assert_eq!(
        *lock(&host.workspaces),
        vec!["scope-0".to_string()],
        "one acquisition, told the executor's workspace id"
    );
    let calls = lock(&host.calls).clone();
    let acquired = calls
        .iter()
        .position(|call| call == "scope_acquired:0")
        .expect("the scope was acquired");
    let first_attempt = calls
        .iter()
        .position(|call| call == "before_attempt:one")
        .expect("the first node was admitted");
    assert!(
        first_attempt < acquired,
        "the environment is acquired once the first firing is admitted: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == "scope_acquired:0")
            .count(),
        1,
        "the second node reuses the environment: {calls:?}"
    );
}

#[tokio::test]
async fn a_host_that_refuses_the_environment_fails_the_scopes_firings_routably() {
    let dir = RunDir::new("scope-acquired-refused");
    let host = Arc::new(SeedingHost {
        refuse: true,
        ..SeedingHost::default()
    });
    let mut b = GraphBuilder::new();
    add_script(&mut b, "one", ScopeId::new(0), "true");
    let report = runtime(&dir, &host)
        .run(b.build())
        .await
        .expect("the run replays");

    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status_of(&report, "one").as_deref(), Some("failure"));
    let failure = report
        .state
        .history()
        .iter()
        .find(|record| record.name == "one")
        .and_then(|record| record.outcome.status.failure_info().cloned())
        .expect("the node failed");
    assert!(
        failure.message.contains("the host refused the workspace"),
        "{}",
        failure.message
    );
    assert!(
        !dir.workspace().join("seed.txt").exists(),
        "nothing was written"
    );
}
