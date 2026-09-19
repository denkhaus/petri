//! Forking a stored run at a position, through the embedding boundary
//! (`host::fork_from`, then `host::resume_configured` over the new run).
//!
//! Every scenario runs a Fabro workflow of real commands to completion,
//! seeds a new run from the finished run's records up to a position, and
//! continues the new run. The new run's own records are what its live
//! projector delivers (the primed projector folds the copied prefix without
//! delivering it), so "what the fork ran" is read off those events, and
//! "what the fork's workspace saw" off the files the commands wrote.

use std::path::Path;
use std::sync::Arc;
use std::{fs, io};

use petri::driver::ExecutionReport;
use petri::engine::Event;
use petri::execution::events::{
    CollectingSink, EventProjector, RunEvent, replay_run, replay_run_dir, verify_export,
    verify_export_run_dir,
};
use petri::execution::host::{self, ForkError, ForkOptions, ForkPosition, HostError, HostRun};
use petri::execution::inspect::{RunInspection, inspect_run, inspect_run_dir};
use petri::execution::{
    Access, CoordinatorEvent, ExecutionObserver, MemoryRunStore, RunKey, RunStore as _,
    open_run_dir,
};
use petri::executor::Retention;
use petri::frontend::CompileInputs;
use petri::ir::{ExecutionId, FiringId, RunStatus};
use petri::{RunOptions, Runtime};
use testkit::RunDir;

// ── Workflows ──────────────────────────────────────────────────────────────

/// Three command stages in a row. Each appends its name to `stages.txt` in
/// the workspace, so the file says which stages ran in which workspace.
const THREE: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    one [shape=parallelogram, script="echo one >> stages.txt; echo one"]
    two [shape=parallelogram, script="echo two >> stages.txt; echo two"]
    three [shape=parallelogram, script="echo three >> stages.txt; echo three"]
    start -> one
    one -> two
    two -> three
    three -> exit
}"#;

/// A command, a fan-out to two branches (child invocations sharing the
/// root's sandbox), a fan-in, and a command after the join.
const PARALLEL: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="echo prepare >> stages.txt; echo prepare"]
    fan [shape=component]
    left [shape=parallelogram, script="echo left >> stages.txt; echo left"]
    right [shape=parallelogram, script="echo right >> stages.txt; echo right"]
    join [shape=tripleoctagon]
    after [shape=parallelogram, script="echo after >> stages.txt; echo after"]
    start -> prepare
    prepare -> fan
    fan -> left
    fan -> right
    left -> join
    right -> join
    join -> after
    after -> exit
}"#;

/// A command that fails, with a failure route and a success route.
const FAILING: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="echo work >> stages.txt; echo boom >&2; exit 3"]
    recover [shape=parallelogram, script="echo recovered >> stages.txt; echo recovered"]
    done [shape=parallelogram, script="echo done >> stages.txt; echo done"]
    start -> work
    work -> recover [condition="outcome=failed"]
    work -> done
    recover -> exit
    done -> exit
}"#;

// ── Helpers ────────────────────────────────────────────────────────────────

fn options(dir: &Path) -> RunOptions {
    let mut options = RunOptions::new(dir);
    // The scenarios read the workspace after the run.
    options.retention = Retention::Always;
    options
}

fn runtime(dir: &Path) -> Runtime {
    petri::runtime().options(options(dir))
}

fn lower(rt: &Runtime, dir: &Path, text: &str) -> HostRun {
    let path = dir.join("wf.fabro");
    fs::write(&path, text).expect("write the workflow");
    let lowered = rt
        .check(&path, None, None, &CompileInputs::new())
        .expect("loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("the workflow lowers: {:?}", lowered.diagnostics));
    HostRun::new(graph).with_children(lowered.children)
}

/// Run `text` to completion in a fresh run directory.
async fn run_source(label: &str, text: &str) -> (RunDir, ExecutionReport) {
    let dir = RunDir::new(label);
    let rt = runtime(dir.path());
    let run = lower(&rt, dir.path(), text);
    let report = host::run_configured(&rt, run, |_, _| {})
        .await
        .expect("the source runs");
    let history: Vec<_> = report
        .state
        .history()
        .iter()
        .map(|record| (record.name.to_string(), record.outcome.status.clone()))
        .collect();
    assert!(
        report.state.errors().is_empty(),
        "{:?}\n{history:?}",
        report.state.errors()
    );
    (dir, report)
}

/// The firing of the node instance `name` in the report's execution.
fn firing_of(report: &ExecutionReport, name: &str) -> FiringId {
    report
        .state
        .history()
        .iter()
        .find(|record| record.name == name)
        .unwrap_or_else(|| panic!("node {name} fired"))
        .firing
}

/// A position in the root's first execution.
fn at(report: &ExecutionReport, name: &str) -> ForkPosition {
    ForkPosition {
        execution: ExecutionId::new(0),
        firing:    firing_of(report, name),
    }
}

/// What a resumed fork did: its report and the events its own records
/// produced.
struct Continued {
    dir:    RunDir,
    key:    RunKey,
    report: ExecutionReport,
    /// The events of the records the fork appended itself.
    live:   Vec<RunEvent>,
}

/// Seed a fork of the run in `source` and continue it in a fresh directory.
async fn fork_and_continue(
    label: &str,
    source: &RunDir,
    position: ForkPosition,
    options: ForkOptions,
) -> Continued {
    let dir = RunDir::new(label);
    let rt = runtime(dir.path());
    let source_logs = open_run_dir(source.path(), Access::Read)
        .await
        .expect("the source opens");
    let forked = host::fork_from(&rt, &*source_logs, position, options)
        .await
        .expect("the fork seeds");
    drop(source_logs);
    assert_eq!(forked.origin.position, position);
    assert_eq!(forked.origin.rerun_last, options.rerun_last);

    // Before it runs, the seeded run is a fork whose position execution has
    // not finished, and its records already export.
    let seeded = inspect_run_dir(dir.path()).await.expect("inspects");
    assert_eq!(seeded.run_key, forked.key);
    assert_eq!(seeded.forked_from.as_ref(), Some(&forked.origin));
    assert!(!seeded.complete, "{seeded:?}");
    assert_eq!(seeded.status, None);
    verify_export_run_dir(dir.path())
        .await
        .expect("the seeded fork exports");

    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::primed_run_dir(sink.clone(), dir.path())
        .await
        .expect("primes");
    let report = host::resume_configured(
        &rt,
        Vec::new(),
        vec![projector.clone() as Arc<dyn ExecutionObserver>],
        |_, _| {},
    )
    .await
    .expect("the fork continues");
    let receipt = projector.shutdown().await;
    assert!(receipt.is_clean(), "{receipt:?}");
    Continued {
        dir,
        key: forked.key,
        report,
        live: sink.events(),
    }
}

/// The node instance names whose `step.started` is among `events`, in order.
fn started(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| matches!(event.engine(), Some(Event::StepStarted { .. })))
        .filter_map(|event| event.subject.as_ref())
        .map(|subject| subject.node.name.to_string())
        .collect()
}

/// The stage names `stages.txt` in the root scope's workspace holds.
fn stages(dir: &Path) -> Vec<String> {
    let path = dir.join("scopes/invocation-0-scope-0/work/stages.txt");
    match fs::read_to_string(&path) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("{}: {error}", path.display()),
    }
}

/// The fork's finished record: complete, exported, replayable, and named
/// as a fork of `source` at `position`.
async fn assert_fork_record(fork: &Continued, source: &RunDir, position: ForkPosition) {
    verify_export_run_dir(fork.dir.path())
        .await
        .expect("the finished fork exports");
    let events = replay_run_dir(fork.dir.path())
        .await
        .expect("the fork replays");
    let source_key = inspect_run_dir(source.path())
        .await
        .expect("inspects")
        .run_key;
    match events.first().and_then(RunEvent::coordinator) {
        Some(CoordinatorEvent::RunStarted {
            key, forked_from, ..
        }) => {
            assert_eq!(*key, fork.key);
            let origin = forked_from.as_ref().expect("the stream says it is a fork");
            assert_eq!(origin.source, source_key);
            assert_eq!(origin.position, position);
        }
        other => panic!("the stream starts with the run declaration: {other:?}"),
    }
    let inspection = inspect_run_dir(fork.dir.path()).await.expect("inspects");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(inspection.status.as_deref(), Some("success"));
    let origin = inspection
        .forked_from
        .expect("the document names the source");
    assert_eq!(origin.source, source_key);
    assert_eq!(origin.position, position);
}

fn invocations(inspection: &RunInspection) -> Vec<(u64, &str)> {
    inspection
        .invocations
        .iter()
        .map(|invocation| (invocation.invocation.raw(), invocation.status))
        .collect()
}

// ── Scenarios ──────────────────────────────────────────────────────────────

/// A three-stage run forked after its first stage: the fork runs the second
/// and third stages in a fresh workspace and nothing else; its stream and
/// its document name the source; its records replay and export.
#[tokio::test]
async fn a_fork_after_the_first_stage_continues_with_the_rest() {
    let (source, report) = run_source("fork-three-source", THREE).await;
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(stages(source.path()), ["one", "two", "three"]);
    let position = at(&report, "one");

    let fork =
        fork_and_continue("fork-three-fork", &source, position, ForkOptions::default()).await;
    assert_eq!(fork.report.status, RunStatus::Success);
    let ran = started(&fork.live);
    assert!(ran.contains(&"two".to_owned()), "{ran:?}");
    assert!(ran.contains(&"three".to_owned()), "{ran:?}");
    assert!(!ran.contains(&"one".to_owned()), "{ran:?}");
    // The fresh workspace saw only the stages the fork ran; the source's
    // workspace is untouched.
    assert_eq!(stages(fork.dir.path()), ["two", "three"]);
    assert_eq!(stages(source.path()), ["one", "two", "three"]);
    // The fork's final state still holds the kept stage.
    let names: Vec<_> = fork
        .report
        .state
        .history()
        .iter()
        .map(|record| record.name.to_string())
        .collect();
    for name in ["one", "two", "three"] {
        assert!(names.contains(&name.to_owned()), "{names:?}");
    }
    assert_fork_record(&fork, &source, position).await;

    let source_inspection = inspect_run_dir(source.path()).await.expect("inspects");
    assert_eq!(source_inspection.forked_from, None);
    let fork_inspection = inspect_run_dir(fork.dir.path()).await.expect("inspects");
    assert_eq!(fork_inspection.graphs, source_inspection.graphs);
    assert_ne!(fork_inspection.run_key, source_inspection.run_key);
    assert_eq!(
        fork_inspection.executions.len(),
        1,
        "the fork continued the execution; nothing restarted"
    );
}

/// A fork at the last position reruns nothing and finishes as the source
/// did; with `rerun_last` it runs the last stage again.
#[tokio::test]
async fn a_fork_at_the_last_position_reruns_nothing_unless_asked() {
    let (source, report) = run_source("fork-last-source", THREE).await;
    let position = at(&report, "three");

    let retry =
        fork_and_continue("fork-last-kept", &source, position, ForkOptions::default()).await;
    assert_eq!(retry.report.status, report.status);
    let ran = started(&retry.live);
    for name in ["one", "two", "three"] {
        assert!(!ran.contains(&name.to_owned()), "{ran:?}");
    }
    assert!(stages(retry.dir.path()).is_empty());
    assert_fork_record(&retry, &source, position).await;

    let rerun = fork_and_continue("fork-last-rerun", &source, position, ForkOptions {
        rerun_last: true,
    })
    .await;
    assert_eq!(rerun.report.status, RunStatus::Success);
    let ran = started(&rerun.live);
    assert!(ran.contains(&"three".to_owned()), "{ran:?}");
    assert!(!ran.contains(&"one".to_owned()), "{ran:?}");
    assert!(!ran.contains(&"two".to_owned()), "{ran:?}");
    assert_eq!(stages(rerun.dir.path()), ["three"]);
    assert_fork_record(&rerun, &source, position).await;
    let inspection = inspect_run_dir(rerun.dir.path()).await.expect("inspects");
    assert!(inspection.forked_from.expect("a fork").rerun_last);
}

/// A run with a parallel fan-out, forked after the fan-in: the branch
/// invocations are kept, finished, in the fork's tree, and only the stage
/// after the join runs. Forked before the fan-out instead, the branches are
/// dropped and the fork runs them again as children of its own.
#[tokio::test]
async fn a_fork_after_a_join_keeps_the_branches_and_before_it_reruns_them() {
    let (source, report) = run_source("fork-parallel-source", PARALLEL).await;
    assert_eq!(report.status, RunStatus::Success);
    let source_inspection = inspect_run_dir(source.path()).await.expect("inspects");
    assert_eq!(invocations(&source_inspection), [
        (0, "finished"),
        (1, "finished"),
        (2, "finished")
    ]);
    let mut source_stages = stages(source.path());
    source_stages.sort();
    assert_eq!(source_stages, ["after", "left", "prepare", "right"]);

    // After the join.
    let joined = at(&report, "join");
    let fork = fork_and_continue(
        "fork-parallel-joined",
        &source,
        joined,
        ForkOptions::default(),
    )
    .await;
    assert_eq!(fork.report.status, RunStatus::Success);
    let ran = started(&fork.live);
    assert!(ran.contains(&"after".to_owned()), "{ran:?}");
    for name in ["prepare", "left", "right"] {
        assert!(!ran.contains(&name.to_owned()), "{ran:?}");
    }
    assert_eq!(stages(fork.dir.path()), ["after"]);
    assert_fork_record(&fork, &source, joined).await;
    let inspection = inspect_run_dir(fork.dir.path()).await.expect("inspects");
    assert_eq!(invocations(&inspection), [
        (0, "finished"),
        (1, "finished"),
        (2, "finished")
    ]);
    // The kept children were copied with their recording times: declared
    // in the source, before the fork's own declaration.
    let events = replay_run_dir(fork.dir.path())
        .await
        .expect("the fork replays");
    let declared_at = events[0].recorded_at;
    let copied: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.coordinator(),
                Some(CoordinatorEvent::InvocationDeclared { invocation, .. })
                    if invocation.raw() != 0
            )
        })
        .collect();
    assert_eq!(copied.len(), 2);
    for event in copied {
        assert!(
            event.recorded_at <= declared_at,
            "a copied record keeps its time: {event:?}"
        );
    }

    // Before the fan-out.
    let prepared = at(&report, "prepare");
    let fork = fork_and_continue(
        "fork-parallel-before",
        &source,
        prepared,
        ForkOptions::default(),
    )
    .await;
    assert_eq!(fork.report.status, RunStatus::Success);
    let ran = started(&fork.live);
    for name in ["left", "right", "after"] {
        assert!(ran.contains(&name.to_owned()), "{ran:?}");
    }
    assert!(!ran.contains(&"prepare".to_owned()), "{ran:?}");
    let mut fork_stages = stages(fork.dir.path());
    fork_stages.sort();
    assert_eq!(fork_stages, ["after", "left", "right"]);
    assert_fork_record(&fork, &source, prepared).await;
    let inspection = inspect_run_dir(fork.dir.path()).await.expect("inspects");
    assert_eq!(
        invocations(&inspection),
        [(0, "finished"), (1, "finished"), (2, "finished")],
        "the branches were declared again by the fork's own fan-out"
    );
    let events = replay_run_dir(fork.dir.path())
        .await
        .expect("the fork replays");
    let declared_at = events[0].recorded_at;
    for event in events.iter().filter(|event| {
        matches!(
            event.coordinator(),
            Some(CoordinatorEvent::InvocationDeclared { invocation, .. }) if invocation.raw() != 0
        )
    }) {
        assert!(
            event.recorded_at >= declared_at,
            "a re-declared child is the fork's own: {event:?}"
        );
    }
}

/// A position inside a branch, an execution the source never had, and a
/// firing the execution never routed are each refused by name.
#[tokio::test]
async fn a_position_inside_a_child_is_refused() {
    let (source, _) = run_source("fork-refused-source", PARALLEL).await;
    let inspection = inspect_run_dir(source.path()).await.expect("inspects");
    let child_execution = inspection
        .invocations
        .iter()
        .find(|invocation| invocation.invocation.raw() == 1)
        .and_then(|invocation| invocation.executions.first().copied())
        .expect("the first branch ran");
    let source_logs = open_run_dir(source.path(), Access::Read)
        .await
        .expect("opens");
    let dir = RunDir::new("fork-refused");
    let rt = runtime(dir.path());

    let inside = host::fork_from(
        &rt,
        &*source_logs,
        ForkPosition {
            execution: child_execution,
            firing:    FiringId::new(1),
        },
        ForkOptions::default(),
    )
    .await
    .expect_err("a position inside a child is refused");
    assert!(
        matches!(
            inside,
            HostError::Fork(ForkError::PositionInChild { execution, invocation })
                if execution == child_execution && invocation.raw() == 1
        ),
        "{inside}"
    );

    let unknown = host::fork_from(
        &rt,
        &*source_logs,
        ForkPosition {
            execution: ExecutionId::new(99),
            firing:    FiringId::new(1),
        },
        ForkOptions::default(),
    )
    .await
    .expect_err("an unknown execution is refused");
    assert!(
        matches!(unknown, HostError::Fork(ForkError::UnknownExecution(execution)) if execution.raw() == 99),
        "{unknown}"
    );

    let unrouted = host::fork_from(
        &rt,
        &*source_logs,
        ForkPosition {
            execution: ExecutionId::new(0),
            firing:    FiringId::new(999),
        },
        ForkOptions::default(),
    )
    .await
    .expect_err("an unrouted firing is refused");
    assert!(
        matches!(
            unrouted,
            HostError::Fork(ForkError::UnroutedFiring { execution, firing })
                if execution.raw() == 0 && firing.raw() == 999
        ),
        "{unrouted}"
    );
    assert!(
        !dir.path().join("run.json").exists(),
        "a refused fork creates no run"
    );
}

/// A fork at a firing that failed keeps the failure and its route: the fork
/// continues on the failure route without running the failed stage again.
#[tokio::test]
async fn a_fork_at_a_failed_firing_continues_on_the_failure_route() {
    let (source, report) = run_source("fork-failure-source", FAILING).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(stages(source.path()), ["work", "recovered"]);
    let position = at(&report, "work");

    let fork = fork_and_continue(
        "fork-failure-fork",
        &source,
        position,
        ForkOptions::default(),
    )
    .await;
    assert_eq!(fork.report.status, report.status);
    let ran = started(&fork.live);
    assert!(ran.contains(&"recover".to_owned()), "{ran:?}");
    assert!(!ran.contains(&"work".to_owned()), "{ran:?}");
    assert!(!ran.contains(&"done".to_owned()), "{ran:?}");
    assert_eq!(stages(fork.dir.path()), ["recovered"]);
    assert_fork_record(&fork, &source, position).await;
    let inspection = inspect_run_dir(fork.dir.path()).await.expect("inspects");
    let engine = inspection.executions[0]
        .engine
        .as_ref()
        .expect("the execution replayed");
    let work = engine
        .history
        .iter()
        .find(|record| record.node == "work")
        .expect("the kept finish");
    assert_eq!(work.status, "failure");
}

/// The same fork over a host's own store: source and fork are runs of one
/// `MemoryRunStore`, named by `RunOptions::run_key`.
#[tokio::test]
async fn a_fork_lives_in_the_runtime_store_under_its_own_key() {
    let store = Arc::new(MemoryRunStore::new());
    let source_dir = RunDir::new("fork-store-source");
    let mut source_options = options(source_dir.path());
    source_options.run_key = Some(RunKey::new("source"));
    let source_rt = petri::runtime()
        .options(source_options)
        .store(store.clone());
    let run = lower(&source_rt, source_dir.path(), THREE);
    let report = host::run_configured(&source_rt, run, |_, _| {})
        .await
        .expect("the source runs");
    assert_eq!(report.status, RunStatus::Success);
    let position = at(&report, "two");

    let fork_dir = RunDir::new("fork-store-fork");
    let mut fork_options = options(fork_dir.path());
    fork_options.run_key = Some(RunKey::new("fork"));
    let fork_rt = petri::runtime().options(fork_options).store(store.clone());
    let source_logs = store
        .open(&RunKey::new("source"), Access::Read)
        .await
        .expect("the source opens");
    let forked = host::fork_from(&fork_rt, &*source_logs, position, ForkOptions::default())
        .await
        .expect("the fork seeds");
    assert_eq!(forked.key, RunKey::new("fork"));
    assert_eq!(forked.origin.source, RunKey::new("source"));

    let resumed = host::resume(&fork_rt).await.expect("the fork continues");
    assert_eq!(resumed.status, RunStatus::Success);
    assert_eq!(stages(fork_dir.path()), ["three"]);
    let fork_logs = store
        .open(&RunKey::new("fork"), Access::Read)
        .await
        .expect("the fork opens");
    verify_export(&*fork_logs).await.expect("exports");
    let events = replay_run(&*fork_logs).await.expect("replays");
    assert!(matches!(
        events.first().and_then(RunEvent::coordinator),
        Some(CoordinatorEvent::RunStarted { forked_from: Some(origin), .. })
            if origin.source == RunKey::new("source") && origin.position == position
    ));
    let inspection = inspect_run(&*fork_logs).await.expect("inspects");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(inspection.run_key, RunKey::new("fork"));
    assert_eq!(inspection.forked_from, Some(forked.origin));
}
