//! The public stream inverts to the records replay consumes: every run's
//! stream rebuilds its coordinator log and each execution's external engine
//! records, and the rebuilt records replay to the same logs and stream.

use std::fs;
use std::path::{Path, PathBuf};

use engine::EventSource;
use execution::events::{EventSource as PublicSource, invert, replay_run, verify_lossless};
use execution::{host, read_engine_log};
use ir::{GraphBuilder, RunStatus, ScopeId};
use runtime::{RunOptions, Runtime};
use testkit::{RunDir, add_script};

#[tokio::test]
async fn a_run_inverts_to_its_external_records_and_replays_to_the_same_stream() {
    let dir = RunDir::new("lossless");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = add_script(&mut b, "first", scope, "echo one; echo two >&2");
    let second = add_script(&mut b, "second", scope, "exit 3");
    b.link(first, second);
    let graph = b.build();

    // The host runs the round trip itself at the end of the run.
    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let report = host::run(&rt, graph).await.expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Failed,
        "{:?}",
        report.state.errors()
    );

    // And it can be run after the fact over the run dir alone.
    let inverted = verify_lossless(dir.path()).expect("the stream inverts and replays");
    let events = replay_run(dir.path()).expect("the run dir projects");
    let coordinator_records = events
        .iter()
        .filter(|event| event.id.index == 0 && matches!(event.id.source, PublicSource::Coordinator))
        .count();
    assert_eq!(inverted.coordinator.len(), coordinator_records);
    assert_eq!(inverted.executions.len(), 1, "one execution ran");
    let records = inverted.executions.values().next().expect("one execution");
    let logs = engine_logs(dir.path());
    assert_eq!(logs.len(), 1, "one engine log: {logs:?}");
    let stored = read_engine_log(&logs[0]).expect("the engine log decodes");
    let external = stored
        .log
        .records()
        .iter()
        .filter(|record| record.source == EventSource::External)
        .count();
    assert_eq!(records.len(), external);
    assert!(
        records.iter().all(|record| record.recorded_at.is_some()),
        "every stored record carries its recording time"
    );

    // `invert` alone is the pure half: the same records from the same events.
    assert_eq!(invert(&events).expect("the events invert"), inverted);
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
