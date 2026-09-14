//! The two in-tree backends pass the store contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use store::{
    Access, COORDINATOR_FILE, LogId, MemoryRunStore, OwnerId, RUN_FILE, RunDirStore, RunKey,
    RunStore, StoreError,
};
use testkit::RunDir;
use testkit::run_store::{conformance, record, stale_owner_conformance};

#[tokio::test]
async fn the_memory_store_conforms() {
    conformance(|| Arc::new(MemoryRunStore::new())).await;
    let store = MemoryRunStore::new();
    stale_owner_conformance(&store, |key| store.release(key)).await;
}

/// A run-directory store serves one run, so each check gets a directory
/// of its own; the directories are removed with the fixtures.
#[tokio::test]
async fn the_run_dir_store_conforms() {
    let dirs: Mutex<Vec<RunDir>> = Mutex::new(Vec::new());
    conformance(|| {
        let dir = RunDir::new("store-conformance");
        let store: Arc<dyn RunStore> = Arc::new(PerKeyRunDirStore::new(dir.path().to_path_buf()));
        dirs.lock().expect("not poisoned").push(dir);
        store
    })
    .await;
}

/// A run-directory store over one directory per key, so the suite's many
/// keys each get a run directory of their own.
struct PerKeyRunDirStore {
    root: PathBuf,
}

impl PerKeyRunDirStore {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait::async_trait]
impl RunStore for PerKeyRunDirStore {
    async fn open(
        &self,
        key: &RunKey,
        access: Access,
    ) -> Result<Arc<dyn store::RunLogs>, StoreError> {
        // One `RunDirStore` per key, kept for the process so the in-process
        // lease share sees a retry by the same owner.
        static STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<RunDirStore>>>> = OnceLock::new();
        let path = self.root.join(key.as_str());
        let store = STORES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .expect("not poisoned")
            .entry(path.clone())
            .or_insert_with(|| Arc::new(RunDirStore::new(path)))
            .clone();
        store.open(key, access).await
    }
}

/// The layout the run directory keeps: `run.json` with the key, the
/// coordinator log as one JSON line per record, and a torn tail truncated
/// by a writer before it appends.
#[tokio::test]
async fn the_run_dir_layout_is_one_line_per_record_and_a_writer_truncates_a_torn_tail() {
    let dir = RunDir::new("store-layout");
    let store = RunDirStore::new(dir.path());
    let key = RunKey::new("layout");
    let owner = OwnerId::new("owner");
    assert_eq!(store.stored_key().expect("reads"), None);
    let logs = store
        .open(&key, Access::Create {
            owner: owner.clone(),
        })
        .await
        .expect("creates");
    assert_eq!(store.stored_key().expect("reads"), Some(key.clone()));
    let run: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(RUN_FILE)).expect("run.json"))
            .expect("json");
    assert_eq!(run["key"], serde_json::json!("layout"));
    logs.append(&LogId::Coordinator, &[record(0, "run.started")])
        .await
        .expect("appends");
    drop(logs);

    let path = dir.path().join(COORDINATOR_FILE);
    let text = fs::read_to_string(&path).expect("the log");
    assert_eq!(text.lines().count(), 1);
    let line: serde_json::Value =
        serde_json::from_str(text.lines().next().expect("a line")).expect("json");
    assert_eq!(
        line,
        record(0, "run.started").record,
        "the line is the record"
    );

    // A torn tail: a reader drops it, a writer truncates it and continues.
    fs::write(&path, format!("{text}{{\"seq\":1,\"recorded_at\":")).expect("writes");
    let reader = store.open(&key, Access::Read).await.expect("reads");
    assert_eq!(
        reader.read(&LogId::Coordinator).await.expect("reads").len(),
        1
    );
    assert!(
        fs::read_to_string(&path)
            .expect("the log")
            .ends_with("\"recorded_at\":"),
        "a reader leaves the file alone"
    );
    let writer = store
        .open(&key, Access::Write { owner })
        .await
        .expect("writes");
    writer
        .append(&LogId::Coordinator, &[record(1, "run.finished")])
        .await
        .expect("appends past the clean prefix");
    let stored = writer.read(&LogId::Coordinator).await.expect("reads");
    assert_eq!(stored, vec![
        record(0, "run.started"),
        record(1, "run.finished")
    ]);
    assert_eq!(
        fs::read_to_string(&path).expect("the log").lines().count(),
        2
    );

    // A key that is not the stored one is not this store's run.
    let error = store
        .open(&RunKey::new("other"), Access::Read)
        .await
        .err()
        .expect("another key is refused");
    assert!(matches!(error, StoreError::NotFound { .. }), "{error}");
}
