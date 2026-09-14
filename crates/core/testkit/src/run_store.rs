//! The store conformance suite: what every [`RunStore`] backend must do.
//!
//! [`conformance`] takes a factory that opens a fresh, empty store and runs
//! the contract against it: the lease rules, the `(log, seq)` rule, read-back
//! equality as JSON values, and the blob round trip. Petri runs it against
//! its run-directory and in-memory backends; a host runs the same function
//! against its own backend, which is what makes that backend checkable
//! without Petri knowledge.
//!
//! The suite ends a lease only by dropping the handle. A backend whose
//! lease can also be released by an operator proves `StaleOwner` through
//! [`stale_owner_conformance`], which takes the release as a closure; a
//! backend whose lease ends with the process alone (a file lock) has no
//! stale owner to prove.

use std::sync::Arc;

use serde_json::{Value, json};
use store::{Access, Digest, LogId, OwnerId, Record, RunKey, RunStore, StoreError};

/// A record with a distinctive body, at `seq`.
pub fn record(seq: u64, body: &str) -> Record {
    Record::from_value(json!({
        "seq": seq,
        "origin": "external",
        "recorded_at": 1_000 + seq,
        "body": { "event": body },
    }))
    .expect("a test record has seq and recorded_at")
}

/// Run the contract against a fresh store from `fresh`. Each check opens
/// its own run, so the checks are independent.
pub async fn conformance(fresh: impl Fn() -> Arc<dyn RunStore>) {
    create_then_write_then_read(&fresh()).await;
    a_second_owner_is_refused_while_the_first_is_live(&fresh()).await;
    a_retry_by_the_same_owner_shares_the_lease(&fresh()).await;
    a_read_handle_refuses_writes(&fresh()).await;
    an_append_retried_after_a_lost_reply_leaves_one_record(&fresh()).await;
    a_different_record_at_a_taken_seq_is_a_conflict(&fresh()).await;
    records_read_back_equal_the_records_written(&fresh()).await;
    blobs_round_trip_by_digest(&fresh()).await;
    a_dropped_handle_ends_the_lease(&fresh()).await;
}

/// The stale-owner rule, for a backend whose lease an operator can end:
/// `release` ends the writer lease of a key from outside, as an operator
/// would.
pub async fn stale_owner_conformance(store: &dyn RunStore, release: impl Fn(&RunKey)) {
    let key = RunKey::new("stale");
    let first = OwnerId::new("first");
    let logs = store
        .open(&key, Access::Create {
            owner: first.clone(),
        })
        .await
        .expect("creates");
    logs.append(&LogId::Coordinator, &[record(0, "run.started")])
        .await
        .expect("the owner appends");
    release(&key);
    let second = OwnerId::new("second");
    let taken = store
        .open(&key, Access::Write { owner: second })
        .await
        .expect("the released run is taken by another owner");
    let error = logs
        .append(&LogId::Coordinator, &[record(1, "late")])
        .await
        .expect_err("the old owner's append is refused");
    assert!(matches!(error, StoreError::StaleOwner), "{error}");
    let stored = taken.read(&LogId::Coordinator).await.expect("reads");
    assert_eq!(stored, vec![record(0, "run.started")]);
    taken
        .append(&LogId::Coordinator, &[record(1, "taken")])
        .await
        .expect("the new owner appends");
}

async fn create_then_write_then_read(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("lifecycle");
    let owner = OwnerId::new("owner");
    let error = store
        .open(&key, Access::Write {
            owner: owner.clone(),
        })
        .await
        .err()
        .expect("a run that does not exist cannot be written");
    assert!(matches!(error, StoreError::NotFound { .. }), "{error}");
    let error = store
        .open(&key, Access::Read)
        .await
        .err()
        .expect("a run that does not exist cannot be read");
    assert!(matches!(error, StoreError::NotFound { .. }), "{error}");

    let created = store
        .open(&key, Access::Create {
            owner: owner.clone(),
        })
        .await
        .expect("creates");
    assert!(!created.locator().is_empty());
    created
        .append(&LogId::Coordinator, &[record(0, "run.started")])
        .await
        .expect("appends");
    let error = store
        .open(&key, Access::Create {
            owner: OwnerId::new("again"),
        })
        .await
        .err()
        .expect("a second create is refused");
    assert!(
        matches!(error, StoreError::Exists { .. } | StoreError::Leased { .. }),
        "{error}"
    );
    drop(created);

    let error = store
        .open(&key, Access::Create {
            owner: OwnerId::new("again"),
        })
        .await
        .err()
        .expect("a create of an existing run is refused after the lease ends");
    assert!(matches!(error, StoreError::Exists { .. }), "{error}");
    let written = store
        .open(&key, Access::Write {
            owner: OwnerId::new("resumer"),
        })
        .await
        .expect("a stored run opens for writing");
    assert_eq!(
        written.read(&LogId::Coordinator).await.expect("reads"),
        vec![record(0, "run.started")]
    );
    written
        .append(&LogId::Coordinator, &[record(1, "run.finished")])
        .await
        .expect("appends after the stored head");
    let reader = store
        .open(&key, Access::Read)
        .await
        .expect("a read never blocks on the lease");
    assert_eq!(
        reader.read(&LogId::Coordinator).await.expect("reads").len(),
        2
    );
    assert!(
        reader
            .read(&LogId::Execution(store::ExecutionId::new(7)))
            .await
            .expect("an absent log reads")
            .is_empty(),
        "a log nothing was appended to is empty"
    );
}

async fn a_second_owner_is_refused_while_the_first_is_live(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("exclusive");
    let first = store
        .open(&key, Access::Create {
            owner: OwnerId::new("first"),
        })
        .await
        .expect("creates");
    let error = store
        .open(&key, Access::Write {
            owner: OwnerId::new("second"),
        })
        .await
        .err()
        .expect("a second owner is refused");
    assert!(matches!(error, StoreError::Leased { .. }), "{error}");
    let reader = store
        .open(&key, Access::Read)
        .await
        .expect("a reader is not refused");
    drop(reader);
    drop(first);
    store
        .open(&key, Access::Write {
            owner: OwnerId::new("second"),
        })
        .await
        .expect("the lease ends with the handle");
}

async fn a_retry_by_the_same_owner_shares_the_lease(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("retry-open");
    let owner = OwnerId::new("owner");
    let first = store
        .open(&key, Access::Create {
            owner: owner.clone(),
        })
        .await
        .expect("creates");
    let again = store
        .open(&key, Access::Write {
            owner: owner.clone(),
        })
        .await
        .expect("a retry after a lost reply recovers the lease");
    again
        .append(&LogId::Coordinator, &[record(0, "run.started")])
        .await
        .expect("the retried handle writes");
    first
        .append(&LogId::Coordinator, &[record(1, "run.finished")])
        .await
        .expect("the first handle still writes");
    let error = store
        .open(&key, Access::Write {
            owner: OwnerId::new("other"),
        })
        .await
        .err()
        .expect("another owner is still refused");
    assert!(matches!(error, StoreError::Leased { .. }), "{error}");
    assert_eq!(
        again.read(&LogId::Coordinator).await.expect("reads").len(),
        2
    );
}

async fn a_read_handle_refuses_writes(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("read-only");
    let writer = store
        .open(&key, Access::Create {
            owner: OwnerId::new("owner"),
        })
        .await
        .expect("creates");
    let reader = store.open(&key, Access::Read).await.expect("reads");
    let error = reader
        .append(&LogId::Coordinator, &[record(0, "run.started")])
        .await
        .expect_err("a read handle cannot append");
    assert!(matches!(error, StoreError::ReadOnly), "{error}");
    let error = reader
        .put_blob(b"graph")
        .await
        .expect_err("a read handle cannot store a blob");
    assert!(matches!(error, StoreError::ReadOnly), "{error}");
    assert!(
        writer
            .read(&LogId::Coordinator)
            .await
            .expect("reads")
            .is_empty()
    );
}

async fn an_append_retried_after_a_lost_reply_leaves_one_record(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("retry-append");
    let logs = store
        .open(&key, Access::Create {
            owner: OwnerId::new("owner"),
        })
        .await
        .expect("creates");
    let log = LogId::Execution(store::ExecutionId::new(0));
    let batch = [record(0, "execution.started"), record(1, "step.started")];
    logs.append(&log, &batch).await.expect("the first append");
    logs.append(&log, &batch)
        .await
        .expect("the same records again are accepted");
    logs.append(&log, &batch[1..])
        .await
        .expect("a partial retry is accepted");
    logs.append(&log, &[
        record(1, "step.started"),
        record(2, "step.finished"),
    ])
    .await
    .expect("a retry that overlaps the head continues past it");
    let stored = logs.read(&log).await.expect("reads");
    assert_eq!(stored, vec![
        record(0, "execution.started"),
        record(1, "step.started"),
        record(2, "step.finished"),
    ]);
}

async fn a_different_record_at_a_taken_seq_is_a_conflict(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("conflict");
    let logs = store
        .open(&key, Access::Create {
            owner: OwnerId::new("owner"),
        })
        .await
        .expect("creates");
    logs.append(&LogId::Resources, &[record(0, "allocating")])
        .await
        .expect("appends");
    let error = logs
        .append(&LogId::Resources, &[record(0, "live")])
        .await
        .expect_err("a different record at a taken seq is refused");
    assert!(
        matches!(error, StoreError::Conflict {
            log: LogId::Resources,
            seq: 0,
        }),
        "{error}"
    );
    let error = logs
        .append(&LogId::Resources, &[record(2, "skips")])
        .await
        .expect_err("a seq past the head is refused");
    assert!(
        matches!(error, StoreError::Conflict {
            log: LogId::Resources,
            seq: 2,
        }),
        "{error}"
    );
    assert_eq!(
        logs.read(&LogId::Resources).await.expect("reads"),
        vec![record(0, "allocating")],
        "a refused append stores nothing"
    );
}

async fn records_read_back_equal_the_records_written(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("read-back");
    let logs = store
        .open(&key, Access::Create {
            owner: OwnerId::new("owner"),
        })
        .await
        .expect("creates");
    // Every value shape a record body can carry: nested objects, arrays,
    // floats that must round-trip, unicode, and null.
    let body = json!({
        "event": "step.finished",
        "outcome": {
            "status": { "kind": "success" },
            "output": [1, 2.5, "three", null, { "four": [4.0e-3] }],
            "text": "héllo — 🌍",
        },
        "recorded": 1.0,
    });
    let mut records = Vec::new();
    for seq in 0..3_u64 {
        let mut value = body.clone();
        value["seq"] = json!(seq);
        value["origin"] = json!("external");
        value["recorded_at"] = json!(5_000 + seq);
        value["outcome"]["index"] = json!(seq);
        records.push(Record::from_value(value).expect("a record"));
    }
    let coordinator = [record(0, "run.started"), record(1, "run.finished")];
    let log = LogId::Execution(store::ExecutionId::new(3));
    logs.append(&log, &records[..2]).await.expect("appends");
    logs.append(&LogId::Coordinator, &coordinator)
        .await
        .expect("appends");
    logs.append(&log, &records[2..]).await.expect("appends");
    let stored = logs.read(&log).await.expect("reads");
    assert_eq!(stored, records, "per log, in order, unchanged");
    for (stored, written) in stored.iter().zip(&records) {
        assert_eq!(stored.seq, written.seq);
        assert_eq!(stored.recorded_at, written.recorded_at);
        let stored: Value = stored.record.clone();
        assert_eq!(stored, written.record, "equal as JSON values");
    }
    assert_eq!(
        logs.read(&LogId::Coordinator).await.expect("reads"),
        coordinator
    );
    let reader = store.open(&key, Access::Read).await.expect("reads");
    assert_eq!(reader.read(&log).await.expect("reads"), records);
}

async fn blobs_round_trip_by_digest(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("blobs");
    let logs = store
        .open(&key, Access::Create {
            owner: OwnerId::new("owner"),
        })
        .await
        .expect("creates");
    let bytes = br#"{"nodes":[],"edges":[]}"#;
    let digest = logs.put_blob(bytes).await.expect("stores");
    assert_eq!(digest, Digest::of(bytes), "the digest is the content's");
    assert_eq!(
        logs.put_blob(bytes).await.expect("stores again"),
        digest,
        "a blob write is idempotent"
    );
    assert_eq!(
        logs.get_blob(digest).await.expect("reads"),
        Some(bytes.to_vec())
    );
    assert_eq!(
        logs.get_blob(Digest::of(b"other")).await.expect("reads"),
        None,
        "an unknown digest is `None`"
    );
    let reader = store.open(&key, Access::Read).await.expect("reads");
    assert_eq!(
        reader.get_blob(digest).await.expect("reads"),
        Some(bytes.to_vec())
    );
}

async fn a_dropped_handle_ends_the_lease(store: &Arc<dyn RunStore>) {
    let key = RunKey::new("drop");
    let owner = OwnerId::new("owner");
    let first = store
        .open(&key, Access::Create {
            owner: owner.clone(),
        })
        .await
        .expect("creates");
    let shared = store
        .open(&key, Access::Write {
            owner: owner.clone(),
        })
        .await
        .expect("shares");
    drop(first);
    let error = store
        .open(&key, Access::Write {
            owner: OwnerId::new("other"),
        })
        .await
        .err()
        .expect("the lease lives while any handle of the owner does");
    assert!(matches!(error, StoreError::Leased { .. }), "{error}");
    drop(shared);
    store
        .open(&key, Access::Write {
            owner: OwnerId::new("other"),
        })
        .await
        .expect("the last handle ends the lease");
}
