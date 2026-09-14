//! The public stream exports its logs: every record's own event carries the
//! stored line, the export check proves it over a run dir, and a crash
//! prefix exports exactly what is stored until a resume writes the rest.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine::{Event, EventOrigin};
use execution::events::{
    CollectingSink, EventId, EventProjector, EventSource, Record, RecordOrigin, RunEvent,
    replay_run, verify_export,
};
use execution::{CoordinatorEvent, host, read_engine_log};
use ir::{GraphBuilder, RunStatus, ScopeId};
use runtime::driver::recorded_now;
use runtime::{RunOptions, Runtime};
use serde_json::Value;
use testkit::{RunDir, add_script};

fn two_steps() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = add_script(&mut b, "first", scope, "echo one; echo two >&2");
    let second = add_script(&mut b, "second", scope, "exit 3");
    b.link(first, second);
    b.build()
}

/// Every `events.jsonl` under `dir`.
fn engine_logs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("the run dir lists") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            found.extend(engine_logs(&path));
        } else if path.file_name().is_some_and(|name| name == "events.jsonl") {
            found.push(path);
        }
    }
    found
}

/// The stored lines of a log, as JSON values; an engine log's header line
/// is not a record.
fn stored_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("reads")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("json"))
        .filter(|line| line.get("seq").is_some())
        .collect()
}

/// A stream's record values by log, in seq order.
fn exported_records(events: &[RunEvent]) -> BTreeMap<EventSource, Vec<Value>> {
    let mut out: BTreeMap<EventSource, Vec<Value>> = BTreeMap::new();
    for event in events {
        if let Some(record) = &event.record {
            assert_eq!(event.id.index, 0, "only a record's own event carries it");
            out.entry(event.id.source)
                .or_default()
                .push(serde_json::to_value(record).expect("encodes"));
        }
    }
    out
}

#[tokio::test]
async fn a_run_s_stream_carries_its_stored_records_unchanged() {
    let dir = RunDir::new("export");
    // The host runs the export check itself at the end of the run.
    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let report = host::run(&rt, two_steps())
        .await
        .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Failed,
        "{:?}",
        report.state.errors()
    );

    // And it can be run after the fact over the run dir alone.
    verify_export(dir.path()).expect("the stream exports its logs");
    let events = replay_run(dir.path()).expect("the run dir projects");
    let exported = exported_records(&events);

    let coordinator = stored_lines(&dir.path().join("coordinator.jsonl"));
    assert_eq!(exported[&EventSource::Coordinator], coordinator);
    let logs = engine_logs(dir.path());
    assert_eq!(logs.len(), 1, "one engine log: {logs:?}");
    let engine = stored_lines(&logs[0]);
    let execution = exported
        .keys()
        .find(|source| matches!(source, EventSource::Execution { .. }))
        .expect("one execution");
    assert_eq!(exported[execution], engine, "every stored line, unchanged");

    // The envelope repeats the record's identity, and the origin is the
    // record's; a view event has no record and is marked derived.
    for event in &events {
        match &event.record {
            Some(Record::Coordinator(record)) => {
                assert_eq!(event.id.seq, record.seq);
                assert_eq!(event.recorded_at, record.recorded_at);
                assert_eq!(event.origin, RecordOrigin::External);
            }
            Some(Record::Engine(record)) => {
                assert_eq!(event.id.seq, record.seq);
                assert_eq!(event.recorded_at, record.recorded_at);
                assert_eq!(event.origin, RecordOrigin::from(record.origin));
            }
            None => {
                assert_eq!(event.origin, RecordOrigin::Derived);
                assert!(event.id.index > 0);
                assert!(event.view().is_some());
            }
        }
    }
    // The core's records are in the stream too, with their origin.
    let stored = read_engine_log(&logs[0]).expect("the engine log decodes");
    let core = stored
        .log
        .records()
        .iter()
        .filter(|record| record.origin == EventOrigin::Core)
        .count();
    assert!(core > 0, "routing emitted core records");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.origin == RecordOrigin::Core)
            .count(),
        core
    );
    // A backend's own payload survives: the record is the whole line.
    assert!(events.iter().any(|event| matches!(
        event.engine(),
        Some(Event::StepProgressRecorded {
            ev: ir::StepEvent::Log { .. },
            ..
        })
    )));
}

/// A crash after an external record is stored but before the core records
/// its apply produced: export holds only the stored prefix with its
/// original times, read-only projection publishes nothing for the missing
/// records, and a resume writes them through the log before its observers
/// see them, with normal recording times, so live and replayed views agree.
#[tokio::test]
async fn a_crash_prefix_exports_what_is_stored_and_resume_stores_the_rest() {
    let dir = RunDir::new("export-crash");
    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let report = host::run(&rt, two_steps())
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);
    let logs = engine_logs(dir.path());
    let log = logs[0].clone();
    let whole = stored_lines(&log);

    // Cut right after the first routing decision: the core records its
    // apply produced (the applied route, the emitted token) are gone, and
    // so is everything after them.
    let text = fs::read_to_string(&log).expect("reads");
    let keep = text
        .lines()
        .position(|line| line.contains("\"routing.resolved\""))
        .expect("a routing decision")
        + 1;
    let prefix: Vec<&str> = text.lines().take(keep).collect();
    fs::write(&log, format!("{}\n", prefix.join("\n"))).expect("writes");
    let stored = stored_lines(&log);
    assert_eq!(stored.len(), keep - 1);
    assert!(stored.len() < whole.len());
    // The coordinator recorded the execution's exit, but the log is a
    // prefix now: complete logs must match exactly, so the check says so.
    let error = verify_export(dir.path()).expect_err("a finished execution's log is short");
    assert!(error.to_string().contains("prefix"), "{error}");
    // Without the exit the prefix is what a crash leaves, and exports.
    let coordinator = dir.path().join("coordinator.jsonl");
    let text = fs::read_to_string(&coordinator).expect("reads");
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            !line.contains("\"execution.finished\"")
                && !line.contains("\"invocation.finished\"")
                && !line.contains("\"run.finished\"")
        })
        .collect();
    fs::write(&coordinator, format!("{}\n", lines.join("\n"))).expect("writes");
    verify_export(dir.path()).expect("a crash prefix exports what it holds");

    let events = replay_run(dir.path()).expect("the run dir projects");
    let exported = exported_records(&events);
    let execution = *exported
        .keys()
        .find(|source| matches!(source, EventSource::Execution { .. }))
        .expect("one execution");
    assert_eq!(
        exported[&execution], stored,
        "only the stored prefix is exported, with its original times"
    );
    let last_stored = u64::try_from(stored.len() - 1).expect("fits");
    assert!(
        events
            .iter()
            .filter(|event| event.id.source == execution)
            .all(|event| event.id.seq <= last_stored),
        "no event is attached to a record the crash kept off disk"
    );

    // Resume with a primed projector: the regenerated suffix and the rest
    // of the run arrive live.
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::primed(sink.clone(), dir.path()).expect("primes");
    let resumed_at = recorded_now();
    let report = host::resume_configured(&rt, Vec::new(), vec![projector.clone()], |_, _| {})
        .await
        .expect("resumes");
    assert_eq!(report.status, RunStatus::Failed);
    let receipt = projector.shutdown().await;
    assert!(receipt.is_clean(), "{receipt:?}");
    let live = sink.events();

    // The stored prefix kept its times; the records the resume wrote have
    // normal recording times.
    let after = stored_lines(&log);
    assert_eq!(&after[..stored.len()], &stored[..]);
    assert!(after.len() > stored.len(), "the resume stored the rest");
    for record in &after[stored.len()..] {
        assert!(record["recorded_at"].as_u64().expect("a time") >= resumed_at);
    }
    // Live delivery started at the first record the crash lost, and every
    // live event equals its replayed twin at the same durable position.
    let first_live = live
        .iter()
        .filter(|event| event.id.source == execution)
        .map(|event| event.id.seq)
        .min()
        .expect("live engine events");
    assert_eq!(first_live, last_stored + 1);
    let replayed: BTreeMap<EventId, RunEvent> = replay_run(dir.path())
        .expect("the resumed run dir projects")
        .into_iter()
        .map(|event| (event.id, event))
        .collect();
    for mut event in live {
        event.observed_at = None;
        assert_eq!(
            replayed.get(&event.id),
            Some(&event),
            "live and replayed views agree at {:?}",
            event.id
        );
    }
    verify_export(dir.path()).expect("the resumed run exports");
    assert!(replayed.values().any(|event| matches!(
        event.coordinator(),
        Some(CoordinatorEvent::RunFinished { .. })
    )));
}
