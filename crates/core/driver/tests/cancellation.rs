//! Handoff §7 tests 3, 4, 5 and 9: the cancellation ladder against real
//! processes.

mod support;

use std::sync::Arc;
use std::time::Duration;

use driver::{CANCEL_FORCED, EventObserver, RunConfig};
use engine::{EngineState, Event, EventRecord};
use executor::{MapSecrets, Retention};
use ir::{CancelScopeId, GraphBuilder, RunStatus, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;
use tokio::sync::Notify;
use tokio::time;

struct WedgedStarted(Notify);

#[async_trait::async_trait]
impl EventObserver for WedgedStarted {
    fn on_record(&self, record: &EventRecord, _state: &EngineState) {
        if matches!(record.event, Event::StepProgress { .. }) {
            self.0.notify_one();
        }
    }
}

/// A script that backgrounds a grandchild, ticks a heartbeat file, and then
/// waits forever. If the group is signalled as a unit, the heartbeat stops.
const BACKGROUNDER: &str = r"
( while :; do echo tick >> heartbeat; sleep 0.05; done ) &
echo ready > ready
sleep 300
";

fn one_step(name: &str, run: &str) -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(name, scope, StepRef::new(PROCESS_KIND, script(run)));
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// §7 test 3, host variant. A step that spawns a background grandchild dies as
/// a unit: every signal goes to the process **group**, never to the child's
/// pid.
#[tokio::test]
async fn cancel_kills_the_whole_process_group() {
    let dir = RunDir::new("group-kill");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(
        one_step("backgrounder", BACKGROUNDER),
        &dir,
        MapSecrets::empty(),
        config,
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await,
        "the step never started"
    );
    let heartbeat = workspace.join("heartbeat");
    assert!(
        wait_for_file(&heartbeat, Duration::from_secs(10)).await,
        "the grandchild never ticked"
    );

    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");
    assert_eq!(report.status, RunStatus::Cancelled);

    // Give anything that survived a chance to prove it.
    time::sleep(Duration::from_millis(600)).await;
    let before = file_len(&heartbeat);
    time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        file_len(&heartbeat),
        before,
        "the backgrounded grandchild outlived its group"
    );
}

/// §7 test 4. A step that traps `SIGTERM`, cleans up and exits inside the grace
/// period is still `Cancelled` — the ladder signalled, so the outcome is ours
/// even though the exit status looks ordinary. Its outputs file is still
/// parsed.
#[tokio::test]
async fn a_step_that_honours_term_still_reports_cancelled() {
    let dir = RunDir::new("term-honoured");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(5))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let script = r#"
trap 'echo cleaned > cleaned.txt; echo "graceful=yes" > "$CI_OUTPUT"; exit 0' TERM
sh -c 'echo ready > ready; exec sleep 300' &
wait
"#;
    let driver = host_driver_with(
        one_step("graceful", script),
        &dir,
        MapSecrets::empty(),
        config,
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "graceful").as_deref(), Some("cancelled"));
    assert!(workspace.join("cleaned.txt").exists(), "the trap never ran");

    let output = output_of(&report, "graceful");
    assert_eq!(
        output["graceful"],
        json!("yes"),
        "a cancelled step's outputs file is still parsed"
    );
    assert_eq!(
        output["cancel_escalation"],
        json!("sigterm"),
        "it went quietly, so the ladder stopped at TERM"
    );
}

/// §7 test 5. A step that ignores `SIGTERM` gets `SIGKILL` once the grace
/// period is up, and the escalation is on the record.
#[tokio::test]
async fn a_step_that_ignores_term_is_killed_after_grace() {
    let dir = RunDir::new("term-ignored");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let script = r"
trap '' TERM
echo ready > ready
while :; do sleep 0.1; done
";
    let driver = host_driver_with(
        one_step("stubborn", script),
        &dir,
        MapSecrets::empty(),
        config,
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "stubborn").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "stubborn")["cancel_escalation"],
        json!("sigkill"),
        "the escalation to KILL is recorded"
    );
}

/// §7 test 9. A step kind that ignores `Control::Cancel` and never returns
/// cannot wedge a run: the driver stops waiting after `grace + slack` and
/// synthesizes the terminal event itself.
#[tokio::test]
async fn a_wedged_step_kind_cannot_wedge_the_run() {
    let dir = RunDir::new("wedged");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node("wedged", scope, StepRef::new(WEDGED_KIND, json!({})));
    let graph = b.build();

    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_millis(300))
        .with_retention(Retention::Never);
    let started = Arc::new(WedgedStarted(Notify::new()));
    let driver = host_driver_full(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig {
            hard_deadline_slack: Duration::from_millis(300),
            ..config
        },
        runners_with_wedged(),
    )
    .observe(started.clone());
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    // The wedged runner logs before waiting for control. Cancel only after
    // that event, regardless of how long its environment takes to acquire.
    time::timeout(Duration::from_secs(10), started.0.notified())
        .await
        .expect("the wedged runner started");
    handle.cancel(CancelScopeId::ROOT).await;

    let report = time::timeout(Duration::from_secs(10), run)
        .await
        .expect("the run must not hang")
        .expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(
        output_of(&report, "wedged")["cancel_escalation"],
        json!(CANCEL_FORCED),
        "the driver recorded that it stopped waiting"
    );
}

/// Cancelling twice joins the ladder already in flight rather than restarting
/// it.
#[tokio::test]
async fn repeated_cancels_join_the_ladder() {
    let dir = RunDir::new("double-cancel");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let script = r"
trap '' TERM
echo ready > ready
while :; do sleep 0.1; done
";
    let driver = host_driver_with(
        one_step("stubborn", script),
        &dir,
        MapSecrets::empty(),
        config,
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    handle.cancel(CancelScopeId::ROOT).await;
    handle.cancel(CancelScopeId::ROOT).await;

    let report = run.await.expect("the run finished");
    assert_eq!(report.status, RunStatus::Cancelled);

    // Exactly one terminal event for the firing, however many cancels arrived.
    assert_one_terminal_per_firing(&report);
}
