//! The `Executor::acquire` fence (§9): a driver crash kills nothing, so
//! re-acquiring a scope must end the prior acquisition's surviving work — or
//! refuse — before anything new runs, and a dead run's status files must never
//! be read as the new attempt's exit. No signal is ever sent to a bare
//! recorded pgid.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use executor::{Executor as _, ExitStatus, ProcessSpec, Retention, ScopeOutcome, ScopeSpec};
use executor_sandbox::HostExecutor;
use ir::ScopeId;
use support::*;
use tokio::time;

fn spec() -> ScopeSpec {
    ScopeSpec::new(ScopeId::new(0), "scope-0")
}

/// Process generations are durable provider records, separate from workspaces.
fn generations(dir: &RunDir) -> Vec<PathBuf> {
    let mut generations = Vec::new();
    if let Ok(resources) = fs::read_dir(dir.path().join("host-registry")) {
        for resource in resources.flatten() {
            if let Ok(entries) = fs::read_dir(resource.path().join("groups")) {
                generations.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
    }
    generations.sort();
    generations
}

async fn settles(path: &Path) -> bool {
    let before = file_len(path);
    time::sleep(Duration::from_millis(500)).await;
    file_len(path) == before
}

/// The crash shape: a survivor still writing into the workspace when a new
/// driver re-acquires. The fence marker makes the surviving group kill itself
/// from inside; the stale status file of the dead generation is not read as
/// the new attempt's exit.
#[tokio::test]
async fn reacquire_fences_the_survivor_and_isolates_status() {
    let dir = RunDir::new("fence-survivor");
    let crashed = HostExecutor::new(dir.path());
    let env = crashed
        .acquire(&spec(), &executor::AcquireContext::bare())
        .await
        .expect("first acquire");
    let exec = env.exec();

    // Something already finished in the crashed run: its recorded status is the
    // stale file a naive resume would misread.
    let mut done = exec
        .spawn(ProcessSpec::new("sh", &["-c", "exit 5"]))
        .await
        .expect("spawn");
    assert_eq!(done.wait().await.expect("wait"), ExitStatus::code(5));

    // And something still running: the survivor.
    let _survivor = exec
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            "while :; do echo tick >> heartbeat; sleep 0.05; done",
        ]))
        .await
        .expect("spawn");
    let heartbeat = dir.workspace().join("heartbeat");
    assert!(wait_for_file(&heartbeat, Duration::from_secs(10)).await);
    // Drop the router without releasing its environment. Its plugin dies,
    // while the sentinel and workload retain their own process group.
    drop(crashed);

    let fresh = HostExecutor::new(dir.path());
    let env2 = fresh
        .acquire(&spec(), &executor::AcquireContext::bare())
        .await
        .expect("the fence clears the way");
    assert!(
        settles(&heartbeat).await,
        "the survivor is dead before the re-acquire returns work"
    );

    // The new acquisition's first spawn gets its own status file: the stale
    // `exit 5` from the dead generation is not read as this one's exit.
    let started = Instant::now();
    let mut fresh_proc = env2
        .exec()
        .spawn(ProcessSpec::new("sh", &["-c", "sleep 0.3; exit 7"]))
        .await
        .expect("spawn");
    assert_eq!(fresh_proc.wait().await.expect("wait"), ExitStatus::code(7));
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the exit was waited for, not read from a stale file"
    );
    fresh.release(env2, ScopeOutcome::Succeeded).await;
}

// Publication ordering and the no-innocent-signal case live in the provider's
// real-plugin recovery tests, where their process records are owned.

/// Fencing is idempotent: acquire, release, acquire again — with work spawned
/// in between — and every fence over the dead generations is a no-op.
#[tokio::test]
async fn fencing_twice_is_a_noop() {
    let dir = RunDir::new("fence-idempotent");
    let executor = HostExecutor::new(dir.path()).with_retention(Retention::Always);

    for _round in 0..3 {
        let env = executor
            .acquire(&spec(), &executor::AcquireContext::bare())
            .await
            .expect("acquire");
        let mut process = env
            .exec()
            .spawn(ProcessSpec::new("sh", &["-c", "exit 0"]))
            .await
            .expect("spawn");
        assert_eq!(process.wait().await.expect("wait"), ExitStatus::code(0));
        executor.release(env, ScopeOutcome::Succeeded).await;
        assert_eq!(
            generations(&dir).len(),
            0,
            "stop removes drained generations after fencing"
        );
    }
}
