//! The standalone host's run dir: `graph.json` + `events.jsonl`, the
//! `JsonlEventLog` battery, the strict read-back rules, and the known-secret
//! refusal.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use petri::driver::EventObserver;
use petri::engine::{self, EngineState, EventRecord, InvalidRecords};
use petri::execution::{self, CoordinatorError};
use petri::executor::docker::{self, RUN_ID_FILE};
use petri::executor::{MapSecrets, Retention, SecretProvider as _};
use petri::host::{self, EVENTS_FILE, EventsDecodeError, GRAPH_FILE, HostError};
use petri::ir::{
    CancelScopeId, Graph, GraphBuilder, RunStatus, RuntimeSpec, Scope, ScopeId, StepRef,
};
use petri::steps::PROCESS_KIND;
use petri::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, add_script, wait_for_file};
use tokio::time;

fn test_runtime(dir: &RunDir) -> Runtime {
    petri::runtime().options(RunOptions::new(dir.path()))
}

fn two_step_graph() -> Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = add_script(&mut b, "first", scope, "echo one");
    let c = add_script(&mut b, "second", scope, "echo two");
    b.link(a, c);
    b.build()
}

/// End to end: both files exist, `graph.json` is the graph byte-exact,
/// `events.jsonl` reads back equal to the in-memory log, and the reloaded pair
/// passes `verify_replay`.
#[tokio::test]
async fn the_run_dir_is_self_describing() {
    let dir = RunDir::new("host-e2e");
    let rt = test_runtime(&dir);
    let graph = two_step_graph();
    let report = host::run(&rt, graph.clone()).await.expect("runs");
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        report.observer_errors.is_empty(),
        "{:?}",
        report.observer_errors
    );

    let graph_bytes = fs::read(dir.path().join(GRAPH_FILE)).expect("graph.json exists");
    assert_eq!(
        graph_bytes,
        serde_json::to_vec(&graph).expect("encodes"),
        "the graph is persisted byte-exact"
    );

    let decoded = host::read_events(&dir.path().join(EVENTS_FILE)).expect("events.jsonl reads");
    assert!(!decoded.torn);
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
        "the file reads back byte-for-byte equal after reserialization"
    );

    let reloaded: Graph = serde_json::from_slice(&graph_bytes).expect("the graph reloads");
    engine::verify_replay(reloaded, &decoded.log).expect("the reloaded pair replays");
}

/// The same framing with no filesystem: encode, decode, byte-equal.
#[tokio::test]
async fn the_byte_codec_round_trips_without_a_filesystem() {
    let dir = RunDir::new("host-codec");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let bytes = host::encode_events(&report.state.log);
    let decoded = host::decode_events(&bytes).expect("decodes");
    assert!(!decoded.torn);
    assert_eq!(decoded.clean_len, bytes.len());
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
    );
}

/// A header from another version is a clean rejection — the standing
/// no-migrator policy, exactly as `EventLog`'s own deserialization behaves.
#[test]
fn a_header_version_mismatch_is_rejected() {
    let bytes = b"{\"version\":4}\n".to_vec();
    match host::decode_events(&bytes) {
        Err(EventsDecodeError::Invalid(InvalidRecords::Version(v))) => {
            assert_eq!(v.found, 4);
        }
        other => panic!("expected a version rejection, got {other:?}"),
    }
}

/// Records whose seqs are not contiguous from 0 cannot become a log.
#[test]
fn out_of_sequence_records_are_rejected() {
    let mut bytes = host::encode_events(&engine::EventLog::new());
    let event = serde_json::to_string(&engine::Event::ExecutionStarted(
        engine::EngineStart::default(),
    ))
    .expect("an event serializes");
    bytes.extend(format!("{{\"seq\":3,\"source\":\"External\",\"event\":{event}}}\n").as_bytes());
    match host::decode_events(&bytes) {
        Err(EventsDecodeError::Invalid(InvalidRecords::SeqMismatch { index, found })) => {
            assert_eq!((index, found), (0, 3));
        }
        other => panic!("expected a seq rejection, got {other:?}"),
    }
}

/// EOF before the final newline: that line is torn, and only that line — the
/// prefix loads, and truncating to `clean_len` yields a clean file.
#[tokio::test]
async fn an_eof_torn_final_line_drops_to_the_prefix() {
    let dir = RunDir::new("host-torn");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let bytes = host::encode_events(&report.state.log);
    let torn = &bytes[..bytes.len() - 10];
    let decoded = host::decode_events(torn).expect("the prefix loads");
    assert!(decoded.torn);
    assert_eq!(decoded.log.len(), report.state.log.len() - 1);

    let clean = &torn[..decoded.clean_len];
    let redecoded = host::decode_events(clean).expect("the clean prefix loads");
    assert!(!redecoded.torn);
    assert_eq!(redecoded.log.len(), decoded.log.len());
}

/// A newline-terminated line that does not decode refuses the load: corruption
/// or tampering is never silently accepted as a crash prefix.
#[tokio::test]
async fn a_terminated_undecodable_line_refuses_the_load() {
    let dir = RunDir::new("host-tamper");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let mut bytes = host::encode_events(&report.state.log);
    bytes.extend(b"not a record\n");
    match host::decode_events(&bytes) {
        Err(EventsDecodeError::BadRecord { line, .. }) => {
            assert_eq!(line, report.state.log.len() + 2);
        }
        other => panic!("expected a record rejection, got {other:?}"),
    }
}

const SECRET: &str = "sk-live-9f3a2b7c1d4e";

/// A run with secrets leaves no secret bytes in either file: the records are
/// post-mask before the battery sees them, and the graph carries references.
#[tokio::test]
async fn the_files_hold_no_secret_bytes() {
    let dir = RunDir::new("host-secrets");
    let provider = MapSecrets::from_pairs(&[("DEPLOY_TOKEN", SECRET)]);
    let rt = petri::runtime()
        .secrets(provider)
        .options(RunOptions::new(dir.path()));

    let mut b = GraphBuilder::new();
    b.add_node(
        "deploy",
        ScopeId::new(0),
        StepRef::new(
            PROCESS_KIND,
            json!({
                "run": r#"echo "token is $DEPLOY_TOKEN""#,
                "env": { "DEPLOY_TOKEN": { "$secret": "DEPLOY_TOKEN" } }
            }),
        ),
    );
    let report = host::run(&rt, b.build()).await.expect("runs");
    assert_eq!(report.status, RunStatus::Success);

    for file in [EVENTS_FILE, GRAPH_FILE] {
        let text = fs::read_to_string(dir.path().join(file)).expect("exists");
        assert!(!text.contains(SECRET), "the secret leaked into {file}");
    }
    let events = fs::read_to_string(dir.path().join(EVENTS_FILE)).expect("exists");
    assert!(events.contains("***"), "the masked line was persisted");
}

/// A registered secret value placed in `Graph.params`: the host refuses to
/// start the run rather than persist the graph.
#[tokio::test]
async fn a_known_secret_in_the_graph_refuses_the_run() {
    let dir = RunDir::new("host-refuse");
    let provider = MapSecrets::empty();
    provider.register("answer:1", SECRET).expect("registers");
    let rt = petri::runtime()
        .secrets(provider)
        .options(RunOptions::new(dir.path()));

    let mut graph = two_step_graph();
    graph
        .params
        .insert(smol_str::SmolStr::new("leak"), json!(SECRET));
    match host::run(&rt, graph).await {
        Err(HostError::SecretInGraph) => {}
        Ok(_) => panic!("the run started with a secret in the graph"),
        Err(other) => panic!("expected the known-secret refusal, got {other}"),
    }
    assert!(
        !dir.path().join(GRAPH_FILE).exists(),
        "nothing was persisted"
    );
}

/// Wait for the marker a script writes, so a stop lands mid-step.
async fn started(dir: &RunDir) {
    assert!(
        wait_for_file(&dir.workspace().join("running"), Duration::from_secs(10)).await,
        "the step never started"
    );
}

/// A cancelled run still leaves complete files: the battery's `finish` is
/// awaited inside the run, whatever way the run ended.
#[tokio::test]
async fn a_cancelled_run_leaves_complete_files() {
    let dir = RunDir::new("host-cancel");
    let rt = test_runtime(&dir);
    let mut b = GraphBuilder::new();
    add_script(
        &mut b,
        "long",
        ScopeId::new(0),
        "echo go > running; sleep 300",
    );

    let driver = host::driver(&rt, b.build()).expect("prepared");
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    started(&dir).await;
    handle.cancel(CancelScopeId::ROOT).await;

    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        report.observer_errors.is_empty(),
        "{:?}",
        report.observer_errors
    );
    let decoded = host::read_events(&dir.path().join(EVENTS_FILE)).expect("reads");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
    );
}

/// So does a killed run.
#[tokio::test]
async fn a_killed_run_leaves_complete_files() {
    let dir = RunDir::new("host-kill");
    let rt = test_runtime(&dir);
    let mut b = GraphBuilder::new();
    add_script(
        &mut b,
        "stubborn",
        ScopeId::new(0),
        "trap '' TERM; echo go > running; while :; do sleep 0.1; done",
    );

    let driver = host::driver(&rt, b.build()).expect("prepared");
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    started(&dir).await;
    handle.cancel(CancelScopeId::ROOT).await;
    handle.cancel(CancelScopeId::ROOT).await;

    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Cancelled);
    let decoded = host::read_events(&dir.path().join(EVENTS_FILE)).expect("reads");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
    );
    assert!(
        decoded
            .log
            .events()
            .any(|e| matches!(e, engine::Event::KillRequested { .. })),
        "the kill tier is in the persisted log"
    );
}

/// Rewrite the root execution's `events.jsonl` — the log resume actually
/// reads, under the coordinator layout — keeping only the first `keep`
/// complete lines (header included), plus `extra` raw bytes: the crash
/// simulator.
fn damage_events(dir: &RunDir, keep: usize, extra: &[u8]) {
    let path = dir
        .path()
        .join("invocations/0000000000000000/executions/0000000000000000")
        .join(EVENTS_FILE);
    let bytes = fs::read(&path).expect("reads");
    let mut end = 0;
    let mut seen = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            seen += 1;
            if seen == keep {
                end = i + 1;
                break;
            }
        }
    }
    let mut out = bytes[..end].to_vec();
    out.extend_from_slice(extra);
    fs::write(&path, out).expect("writes");
}

/// A crashed run resumes from nothing but the run dir, and the file converges
/// to the resumed run's log without rewriting history.
#[tokio::test]
async fn a_crashed_run_resumes_from_the_run_dir() {
    let dir = RunDir::new("host-resume");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");
    let total = report.state.log.len();

    // Drop the final record — the second step's finish — at a line boundary:
    // the arbitrary-tail loss window, in its cleanest shape.
    damage_events(&dir, total, b"");
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(resumed.status, RunStatus::Success);
    assert!(
        resumed.observer_errors.is_empty(),
        "{:?}",
        resumed.observer_errors
    );

    let decoded = host::read_events(&dir.path().join(EVENTS_FILE)).expect("reads");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&resumed.state.log).expect("encodes"),
        "the file converged to the resumed log"
    );
}

/// A torn tail — EOF mid-record — is truncated away and the run resumes from
/// the prefix.
#[tokio::test]
async fn a_torn_tail_is_truncated_and_resumed() {
    let dir = RunDir::new("host-resume-torn");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");
    let total = report.state.log.len();

    damage_events(&dir, total, b"{\"seq\":9999,\"source\":\"Ext");
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(resumed.status, RunStatus::Success);

    let decoded = host::read_events(&dir.path().join(EVENTS_FILE)).expect("reads clean");
    assert!(!decoded.torn, "the torn tail is gone");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&resumed.state.log).expect("encodes"),
    );
}

/// A complete but undecodable line refuses the resume outright.
#[tokio::test]
async fn an_undecodable_record_refuses_resume() {
    let dir = RunDir::new("host-resume-refuse");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    damage_events(
        &dir,
        report.state.log.len(),
        b"corrupted beyond recognition\n",
    );
    match host::resume(&rt).await {
        Err(HostError::Coordinator(CoordinatorError::EngineLog(
            execution::EngineLogError::Decode { source, .. },
        ))) => {
            assert!(matches!(source, EventsDecodeError::BadRecord { .. }));
        }
        Ok(_) => panic!("a corrupted file resumed"),
        Err(other) => panic!("expected the record refusal, got {other}"),
    }
}

/// An observer registered on the `Runtime` builder reaches the driver it
/// builds, alongside the host's own battery.
#[derive(Default)]
struct Counting {
    seen: Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl EventObserver for Counting {
    fn on_record(&self, record: &EventRecord, _state: &EngineState) {
        self.seen.lock().expect("not poisoned").push(record.seq);
    }
}

#[tokio::test]
async fn a_runtime_registered_observer_reaches_the_driver() {
    let dir = RunDir::new("host-runtime-observe");
    let counting = Arc::new(Counting::default());
    let rt = test_runtime(&dir).observe(counting.clone() as Arc<dyn EventObserver>);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let seen = counting.seen.lock().expect("not poisoned");
    let expected: Vec<u64> = report.state.log.records().iter().map(|r| r.seq).collect();
    assert_eq!(*seen, expected);
}

/// The Docker fence across a resume, through the host: `host::resume` builds a
/// fresh executor, which rebuilds the crashed run's container names from the
/// run id recorded in the run dir and ends the container before re-dispatching
/// the step.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "the skip notice tells whoever runs the tests why this Docker battery did nothing; a test binary has no other sink"
)]
async fn resume_fences_the_crashed_container() {
    if !testkit::is_docker_available().await {
        eprintln!("skipping: no Docker daemon reachable");
        return;
    }
    let dir = RunDir::new("host-docker-resume");
    // The workspace is kept: release would otherwise remove the directory the
    // heartbeat is checked through, hiding a beater the fence missed.
    let mut options = RunOptions::new(dir.path());
    options.retention = Retention::Always;
    let rt = petri::runtime().options(options);
    let mut b = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("alpine:3.20");
    let scope = b.add_scope(scope);
    // The beater runs in its own session: an aborted driver still lets the
    // orphaned step task stop its own process group as the channels close, so a
    // detached beater is what a dead *process* leaves behind — only a
    // container-level fence can end it. With `done` already in the workspace
    // the step exits at once, so the resumed run completes instead of beating.
    b.add_node(
        "beat",
        scope,
        StepRef::new(
            PROCESS_KIND,
            testkit::script_with(
                "[ -e done ] && exit 0\n\
                 setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' &\n\
                 sleep 300",
                &json!({ "shell": "sh" }),
            ),
        ),
    );
    let graph = b.build();
    let workspace = dir.workspace();
    let heartbeat = workspace.join("heartbeat");

    let driver = host::driver(&rt, graph).expect("prepared");
    let run = tokio::spawn(driver.run());
    assert!(
        wait_for_file(&heartbeat, Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    // The crash: the driver is gone, release never runs, the container beats on.
    run.abort();
    let _ = run.await;
    // The crashed run's id, as a resuming process must find it.
    let run_id =
        fs::read_to_string(dir.path().join(RUN_ID_FILE)).expect("the run id is in the run dir");
    let prefix = format!("petri-{run_id}-");

    fs::write(workspace.join("done"), b"").expect("done");
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );

    let before = testkit::file_len(&heartbeat);
    time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        testkit::file_len(&heartbeat),
        before,
        "the crashed container kept writing: the fence missed it"
    );
    let leftovers = docker::list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}
