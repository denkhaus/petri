use std::fs;

use execution::{COORDINATOR_FILE, CoordinatorStore, StoreError, decode_coordinator_log};
use runtime::store::RunKey;
use testkit::RunDir;

#[test]
fn the_run_directory_has_one_exclusive_lease() {
    let directory = RunDir::new("coordinator-lease");
    let store = CoordinatorStore::create(directory.path(), RunKey::new("test"), Vec::new())
        .expect("first lease");
    let Err(error) = CoordinatorStore::resume(directory.path()) else {
        panic!("second lease must be refused");
    };
    assert!(matches!(error, StoreError::Leased(_)));
    drop(store);
    CoordinatorStore::resume(directory.path()).expect("lease releases on drop");
}

#[test]
fn a_torn_final_coordinator_record_drops_to_the_complete_prefix() {
    let directory = RunDir::new("coordinator-torn");
    let path = directory.path().join(COORDINATOR_FILE);
    let store =
        CoordinatorStore::create(directory.path(), RunKey::new("test"), Vec::new()).expect("store");
    drop(store);
    let mut bytes = fs::read(&path).expect("log");
    bytes.extend_from_slice(b"{\"seq\":1,\"event\":");
    let decoded = decode_coordinator_log(&path, &bytes).expect("complete prefix decodes");
    assert!(decoded.torn);
    assert_eq!(decoded.records.len(), 1);
}

#[test]
fn a_complete_invalid_coordinator_record_is_corruption() {
    let directory = RunDir::new("coordinator-corrupt");
    let path = directory.path().join(COORDINATOR_FILE);
    let store =
        CoordinatorStore::create(directory.path(), RunKey::new("test"), Vec::new()).expect("store");
    drop(store);
    let mut bytes = fs::read(&path).expect("log");
    bytes.extend_from_slice(b"not-json\n");
    let error = decode_coordinator_log(&path, &bytes).expect_err("complete corruption is refused");
    assert!(matches!(error, StoreError::BadRecord { line: 2, .. }));
}
