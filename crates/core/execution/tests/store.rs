//! The coordinator's record over the run-directory store: one exclusive
//! lease, a torn tail dropped by the store, and complete corruption refused.

use std::fs;

use execution::{
    Access, COORDINATOR_FILE, OwnerId, RunDirStore, RunKey, RunStore as _, StoreError, host,
    read_coordinator_log,
};
use runtime::store::StoreError as BackendError;
use runtime::{RunOptions, Runtime};
use testkit::RunDir;

/// A run the coordinator created and finished, for the tests to reopen.
async fn finished_run(label: &str) -> RunDir {
    let directory = RunDir::new(label);
    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut builder = ir::GraphBuilder::new();
    builder.add_step("only", ir::ScopeId::new(0), "noop");
    host::run(&runtime, builder.build())
        .await
        .expect("the run completes");
    directory
}

#[tokio::test]
async fn the_run_directory_has_one_exclusive_lease() {
    let directory = Box::pin(finished_run("coordinator-lease")).await;
    let store = RunDirStore::new(directory.path());
    let key = store.stored_key().expect("reads").expect("a run");
    let first = store
        .open(&key, Access::Write {
            owner: OwnerId::new("first"),
        })
        .await
        .expect("first lease");
    let error = store
        .open(&key, Access::Write {
            owner: OwnerId::new("second"),
        })
        .await
        .err()
        .expect("second lease must be refused");
    assert!(matches!(error, BackendError::Leased { .. }), "{error}");
    drop(first);
    store
        .open(&key, Access::Write {
            owner: OwnerId::new("second"),
        })
        .await
        .expect("lease releases on drop");
}

#[tokio::test]
async fn a_torn_final_coordinator_record_drops_to_the_complete_prefix() {
    let directory = Box::pin(finished_run("coordinator-torn")).await;
    let path = directory.path().join(COORDINATOR_FILE);
    let whole = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("the log decodes");
    let mut bytes = fs::read(&path).expect("log");
    bytes.extend_from_slice(b"{\"seq\":99,\"recorded_at\":");
    fs::write(&path, &bytes).expect("writes");
    let records = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("the complete prefix decodes");
    assert_eq!(records, whole, "the torn line is dropped, nothing else");
}

#[tokio::test]
async fn a_complete_invalid_coordinator_record_is_corruption() {
    let directory = Box::pin(finished_run("coordinator-corrupt")).await;
    let path = directory.path().join(COORDINATOR_FILE);
    let mut bytes = fs::read(&path).expect("log");
    bytes.extend_from_slice(b"not-json\n");
    fs::write(&path, &bytes).expect("writes");
    let error = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect_err("complete corruption is refused");
    assert!(
        matches!(error, StoreError::Store(BackendError::Backend { .. })),
        "{error}"
    );
    // A run directory that was never a run: no key, no run.
    let empty = RunDir::new("coordinator-not-a-run");
    let error = RunDirStore::new(empty.path())
        .open_stored(Access::Read)
        .await
        .err()
        .expect("no run.json");
    assert!(matches!(error, BackendError::Backend { .. }), "{error}");
    let _ = RunKey::new("unused");
}
