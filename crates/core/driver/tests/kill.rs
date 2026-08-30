//! The two-tier stop wiring (§10): the first root cancel is polite and admits
//! `run_on_cancel` cleanup; the cleanup-grace timer — or a second cancel —
//! feeds `KillRequested`, and after a kill nothing starts and nothing waits out
//! a grace.

mod support;

use std::time::Duration;

use driver::RunConfig;
use engine::{Event, EventSource};
use executor::Retention;
use ir::{CancelScopeId, GraphBuilder, RunStatus, ScopeId};
use serde_json::json;
use support::*;

/// work → cleanup, with cleanup opted in to run after a cancel.
fn cleanup_graph(cleanup_script: &str) -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let work = add_script(&mut b, "work", scope, "echo ready > ready && sleep 300");
    let cleanup = add_script(&mut b, "cleanup", scope, cleanup_script);
    b.link(work, cleanup);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    ir::validate(&graph).expect("valid");
    graph
}

/// The log's one `KillRequested`, which must be External, with nothing starting
/// after it.
fn assert_kill_in_log(report: &driver::RunReport) {
    let kills: Vec<u64> = report
        .state
        .log
        .records()
        .iter()
        .filter(|r| matches!(r.event, Event::KillRequested { .. }))
        .map(|r| {
            assert_eq!(
                r.source,
                EventSource::External,
                "the kill is in the log as External"
            );
            r.seq
        })
        .collect();
    assert_eq!(kills.len(), 1, "exactly one KillRequested");
    let kill_seq = kills[0];
    assert!(
        !report
            .state
            .log
            .records()
            .iter()
            .any(|r| r.seq > kill_seq && matches!(r.event, Event::StepStarted { .. })),
        "no step starts after the kill"
    );
}

/// §6 driver test 1: a real cleanup process executes after a cancel — pinned by
/// which step started, not just by the file it left behind.
#[tokio::test]
async fn cleanup_runs_after_a_cancel() {
    let dir = RunDir::new("cleanup-after-cancel");
    let graph = cleanup_graph("echo cleaned > cleaned.txt");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(graph.clone(), &dir, executor::MapSecrets::empty(), config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "work").as_deref(), Some("cancelled"));
    assert!(
        started(&report).iter().any(|n| n == "cleanup"),
        "the cleanup step started for real: {:?}",
        started(&report)
    );
    assert_eq!(status_of(&report, "cleanup").as_deref(), Some("success"));
    assert!(workspace.join("cleaned.txt").exists());
    assert_replay_identical(&graph, &report);
}

/// §6 driver test 2, first half: cleanup-grace expiry feeds `KillRequested`.
/// The cleanup ignores TERM, and the grace between TERM and KILL is set long,
/// so only an immediate `SIGKILL` — no ladder, no grace wait — explains the
/// timing.
#[tokio::test]
async fn cleanup_grace_expiry_feeds_kill() {
    let dir = RunDir::new("cleanup-grace-kill");
    let graph = cleanup_graph("trap '' TERM; echo x > cleanup-started; sleep 300");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(10))
        .with_cleanup_grace(Duration::from_secs(2))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(graph.clone(), &dir, executor::MapSecrets::empty(), config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    let cancelled_at = std::time::Instant::now();
    handle.cancel(CancelScopeId::ROOT).await;
    let report = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the kill ends the run")
        .expect("the run finished");
    let elapsed = cancelled_at.elapsed();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        workspace.join("cleanup-started").exists(),
        "the cancel admitted the cleanup before the kill ended it"
    );
    assert_eq!(status_of(&report, "cleanup").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "cleanup")["cancel_escalation"],
        json!("sigkill"),
        "Control::Kill went straight to SIGKILL"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "no 10s TERM grace was waited out after the kill: {elapsed:?}"
    );
    assert_kill_in_log(&report);
    assert_one_terminal_per_firing(&report);
    assert_replay_identical(&graph, &report);
}

/// §6 driver test 2, second half: a second root cancel feeds `KillRequested`
/// without waiting for the cleanup grace.
#[tokio::test]
async fn a_second_cancel_feeds_kill() {
    let dir = RunDir::new("second-cancel-kill");
    let graph = cleanup_graph("trap '' TERM; echo x > cleanup-started; sleep 300");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(10))
        .with_cleanup_grace(Duration::from_secs(300))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(graph.clone(), &dir, executor::MapSecrets::empty(), config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    assert!(
        wait_for_file(&workspace.join("cleanup-started"), Duration::from_secs(10)).await,
        "the polite cancel admitted the cleanup"
    );
    let killed_at = std::time::Instant::now();
    handle.cancel(CancelScopeId::ROOT).await;
    let report = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the second cancel ends the run without waiting the 300s cleanup grace")
        .expect("the run finished");
    let elapsed = killed_at.elapsed();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(
        output_of(&report, "cleanup")["cancel_escalation"],
        json!("sigkill")
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "the kill skipped the TERM grace: {elapsed:?}"
    );
    assert_kill_in_log(&report);
    assert_one_terminal_per_firing(&report);
    assert_replay_identical(&graph, &report);
}
