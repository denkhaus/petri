//! The `Executor::acquire` fence (§9): a driver crash kills nothing, so
//! re-acquiring a scope must end the prior acquisition's surviving work — or
//! refuse — before anything new runs, and a dead run's status files must never
//! be read as the new attempt's exit. No signal is ever sent to a bare
//! recorded pgid.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use executor::{EnvError, Executor, ExitStatus, ProcessSpec, ScopeOutcome, ScopeSpec};
use executor_host::HostExecutor;
use ir::ScopeId;
use support::*;
use tokio::process::Command;
use tokio::time;

fn spec() -> ScopeSpec {
    ScopeSpec::new(ScopeId::new(0), "scope-0")
}

fn groups_root(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes").join("scope-0").join("groups")
}

/// The generation dirs currently under the scope, sorted by name.
fn generations(dir: &RunDir) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(groups_root(dir))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
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
    // `env`, `crashed` and the process handles are deliberately never released:
    // the driver process died.

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

/// The publication race, from the sentinel's side: a generation fenced between
/// OS spawn and publication never starts its workload — the sentinel publishes,
/// meets the marker at its check, and exits.
#[tokio::test]
async fn a_prefenced_generation_never_starts_the_workload() {
    let dir = RunDir::new("fence-prefenced");
    let executor = HostExecutor::new(dir.path());
    let env = executor
        .acquire(&spec(), &executor::AcquireContext::bare())
        .await
        .expect("acquire");

    let gen_dirs = generations(&dir);
    assert_eq!(gen_dirs.len(), 1, "one generation per acquisition");
    fs::write(gen_dirs[0].join("fenced"), b"").expect("the marker");

    let mut process = env
        .exec()
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            "echo started > started; sleep 300",
        ]))
        .await
        .expect("spawn");
    // The group ends on its own — the sentinel exited at its check — and the
    // workload never ran.
    let status = time::timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("the group ends without outside help")
        .expect("wait");
    assert_eq!(status, ExitStatus::signalled(SIGKILL));
    assert!(
        !dir.workspace().join("started").exists(),
        "a fenced generation can never start a workload"
    );
    executor.release(env, ScopeOutcome::Succeeded).await;
}

/// SIGKILL's number without linking libc into this test crate.
const SIGKILL: i32 = 9;

/// A group whose in-group kill mechanism is gone — a dead sentinel with a
/// surviving workload, or a recorded pgid that now belongs to someone else
/// entirely (the fencer cannot tell, which is the point): acquire fails with
/// the typed leak error and **no signal is sent**.
#[tokio::test]
async fn an_unkillable_group_fails_acquire_without_a_signal() {
    let dir = RunDir::new("fence-leak");
    // A live process group with no sentinel and no watcher: nothing inside it
    // will ever honor the marker.
    let mut rogue = Command::new("sh");
    rogue
        .arg("-c")
        .arg("while :; do echo tick >> rogue-heartbeat; sleep 0.05; done")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut rogue = rogue.spawn().expect("spawn");
    let pgid = rogue.id().expect("pid").cast_signed();
    let heartbeat = dir.path().join("rogue-heartbeat");
    assert!(wait_for_file(&heartbeat, Duration::from_secs(10)).await);

    let fake = groups_root(&dir).join("gone");
    fs::create_dir_all(&fake).expect("fake generation");
    fs::write(fake.join("0.group"), format!("{pgid}\n")).expect("record");

    let executor = HostExecutor::new(dir.path()).with_fence_drain(Duration::from_millis(300));
    match executor
        .acquire(&spec(), &executor::AcquireContext::bare())
        .await
    {
        Err(EnvError::FenceLeaked { generation, .. }) => {
            assert_eq!(generation, "gone");
        }
        Ok(_) => panic!("acquire succeeded over a leaked group"),
        Err(other) => panic!("expected the typed leak error, got {other}"),
    }

    // The decisive half: nothing was signalled. The rogue group still beats.
    let before = file_len(&heartbeat);
    time::sleep(Duration::from_millis(300)).await;
    assert!(
        file_len(&heartbeat) > before,
        "no signal is ever sent to a bare recorded pgid"
    );
    let _ = rogue.kill().await;
}

/// Fencing is idempotent: acquire, release, acquire again — with work spawned
/// in between — and every fence over the dead generations is a no-op.
#[tokio::test]
async fn fencing_twice_is_a_noop() {
    let dir = RunDir::new("fence-idempotent");
    let executor = HostExecutor::new(dir.path());

    for round in 0..3 {
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
            round + 1,
            "one generation dir per acquisition"
        );
    }
}
