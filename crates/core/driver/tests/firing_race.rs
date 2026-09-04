//! Regression guard for the corpus sweeps' `engine error: unknown firing
//! FiringId(n)`.
//!
//! The driver used to dispatch `Command::StartStep` by spawning the runner task
//! first and only afterwards sending `Signal::Inject(Event::StepStarted)`
//! through the signal channel. A runner that returns immediately — `noop`, a
//! gated-out step — raced its spawned `Signal::Finished` send against that
//! `StepStarted` send. When `Finished` won, the engine finalized the firing and
//! removed it from the live map, and the late `StepStarted` reported
//! `RunError::UnknownFiring`, which failed the run with no failing step record.
//! `dispatch` now applies `StepStarted` synchronously before the runner can
//! exist, so the log order is structural, not a race.

mod support;

use std::sync::Arc;

use driver::{Driver, RunConfig};
use engine::{Admission, Command, EngineState, Event, apply};
use executor::{Executor, MapSecrets};
use executor_sandbox::HostExecutor;
use ir::{GraphBuilder, Outcome, RunStatus, ScopeId, validate};
use steps::{NOOP_KIND, NoopStep, Registry};
use support::*;

/// The engine half, deterministic: the exact event order the racy driver can
/// produce — `StepFinished` arriving before its firing's `StepStarted` — pushes
/// `UnknownFiring` and fails the run, while the step's own outcome is recorded
/// as a success. That is the corpus signature: a failed run with `(run)` and no
/// failing step record.
#[test]
fn finished_before_started_is_unknown_firing() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = b.add_step("first", scope, NOOP_KIND);
    let second = b.add_step("second", scope, NOOP_KIND);
    b.link(first, second);
    let graph = b.build();
    validate(&graph).expect("valid");

    let state = EngineState::new(graph);
    let (state, commands) = apply(
        state,
        Event::ExecutionStarted(engine::EngineStart::default()),
    );
    let decision_id = commands
        .iter()
        .find_map(|command| match command {
            Command::Admit { decision_id } => Some(*decision_id),
            _ => None,
        })
        .expect("execution start asks for admission");
    let (state, commands) = apply(state, Event::Admitted {
        decision_id,
        decision: Admission::Admit,
        trace: Vec::new(),
    });
    let decision_id = commands
        .iter()
        .find_map(|command| match command {
            Command::Admit { decision_id } => Some(*decision_id),
            _ => None,
        })
        .expect("the first attempt asks for admission");
    let (state, commands) = apply(state, Event::Admitted {
        decision_id,
        decision: Admission::Admit,
        trace: Vec::new(),
    });
    let (firing, attempt) = commands
        .iter()
        .find_map(|c| match c {
            Command::StartStep(r) => Some((r.id(), r.attempt())),
            _ => None,
        })
        .expect("the entry node was dispatched");

    // The driver's racy order: the runner's Finished beats the StepStarted send.
    let (state, _) = apply(state, Event::StepFinished {
        firing,
        attempt,
        outcome: Outcome::success(ir::Value::Null),
    });
    assert!(state.errors().is_empty(), "{:?}", state.errors());
    let (state, _) = apply(state, Event::StepStarted { firing, attempt });

    assert_eq!(
        state.errors(),
        &[engine::RunError::UnknownFiring(firing)],
        "the late StepStarted is the unknown-firing report"
    );
    assert_eq!(state.folded_status(), RunStatus::Failed);
    // The firing itself recorded a clean success: no failing step record exists,
    // which is why the corpus report falls back to the `(run)` label.
    assert!(
        state
            .history()
            .iter()
            .all(|r| !r.outcome.status.is_failure()),
        "no failing record"
    );
}

/// The driver half, stochastic: many instant `noop` firings per run, many runs,
/// on a multi-thread runtime. Any run whose engine errors are non-empty is the
/// race firing. Before the fix this failed in ~399/400 runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn instant_steps_never_race_their_step_started() {
    const RUNS: usize = 400;
    const WIDTH: usize = 48;

    let mut hits: Vec<(usize, Vec<engine::RunError>)> = Vec::new();
    for i in 0..RUNS {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let root = b.add_step("root", scope, NOOP_KIND);
        let mut firsts = Vec::new();
        for k in 0..WIDTH {
            let a = b.add_step(&format!("a{k}"), scope, NOOP_KIND);
            let z = b.add_step(&format!("z{k}"), scope, NOOP_KIND);
            b.link(a, z);
            firsts.push(a);
        }
        b.fan_out(root, &firsts);
        let graph = b.build();
        validate(&graph).expect("valid");

        let dir = RunDir::new("firing-race");
        let mut registry = Registry::new();
        registry.register(NoopStep);
        let executor: Arc<dyn Executor> = Arc::new(HostExecutor::new(dir.path()));
        let driver = Driver::new(
            graph,
            executor,
            registry,
            Arc::new(MapSecrets::empty()),
            RunConfig::new(dir.path()),
        );
        let report = driver.run().await;
        if report.state.errors().is_empty() {
            assert_eq!(report.status, RunStatus::Success);
        } else {
            hits.push((i, report.state.errors().to_vec()));
        }
    }

    assert!(
        hits.is_empty(),
        "the StepStarted race fired in {}/{RUNS} runs; first hits: {:?}",
        hits.len(),
        hits.iter().take(3).collect::<Vec<_>>()
    );
}
