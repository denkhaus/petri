//! Handoff §7 tests 3 and 10, Docker halves. These skip when no daemon is reachable.
//!
//! The one that matters most is `docker_cancel_kills_the_exec_process_group`:
//! `docker kill` signals PID 1 and never reaches an exec'd step, so cancellation has
//! to be `docker exec … kill -- -PGID`. Getting that wrong is the classic bug, and
//! this is the test aimed at it.

mod support;

use std::time::Duration;

use driver::RunConfig;
use executor::Retention;
use executor_docker::list_containers;
use ir::{GraphBuilder, RunStatus, RuntimeSpec, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;
use testkit::docker_ready;

const IMAGE: &str = "alpine:3.20";

fn docker_graph(name: &str, run: &str) -> ir::Graph {
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container(IMAGE);
    let scope = b.add_scope(scope);
    b.add_node(
        name,
        scope,
        StepRef::new(PROCESS_KIND, script_with(run, json!({ "shell": "sh" }))),
    );
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// A step runs inside the container, against the bind-mounted workspace.
#[tokio::test]
async fn a_step_runs_inside_the_container() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-basic");
    let graph = docker_graph(
        "inside",
        r#"echo "running on $(uname -s)"; echo "where=$(pwd)" > "$CI_OUTPUT""#,
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let report = docker_driver(graph, &dir, config).await_run().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(
        log_lines(&report).iter().any(|l| l == "running on Linux"),
        "{:?}",
        log_lines(&report)
    );
    assert_eq!(
        output_of(&report, "inside")["where"],
        json!("/workspace"),
        "the workspace is bind-mounted at a known path"
    );
}

/// §7 test 3, Docker variant, and the §4.1 test.
///
/// The step backgrounds a grandchild inside the container. `docker kill` would
/// signal PID 1 and leave both alive; only `docker exec … kill -- -PGID` reaches
/// them. The heartbeat file is on the bind mount, so the host can watch it stop.
#[tokio::test]
async fn docker_cancel_kills_the_exec_process_group() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-group-kill");
    let graph = docker_graph(
        "backgrounder",
        r#"
( while :; do echo tick >> heartbeat; sleep 0.05; done ) &
echo ready > ready
sleep 300
"#,
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = docker_driver(graph, &dir, config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_file(&workspace.join("ready"), Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    let heartbeat = workspace.join("heartbeat");
    assert!(
        wait_for_file(&heartbeat, Duration::from_secs(30)).await,
        "the grandchild never ticked"
    );

    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(
        status_of(&report, "backgrounder").as_deref(),
        Some("cancelled")
    );

    // This step does not trap TERM, so TERM alone must have ended it. If the signal
    // had not reached the group the ladder would have waited out the whole grace
    // period and escalated — and the heartbeat check below would then pass for the
    // wrong reason, because release removes the container either way.
    assert_eq!(
        output_of(&report, "backgrounder")["cancel_escalation"],
        json!("sigterm"),
        "the signal never reached the exec's process group"
    );

    // If the signal had gone to PID 1 instead of the step's group, the grandchild
    // would still be ticking here.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = file_len(&heartbeat);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        file_len(&heartbeat),
        before,
        "the grandchild survived: the signal did not reach the exec's process group"
    );
}

/// §7 test 10, Docker half. Release leaves no container behind.
#[tokio::test]
async fn docker_release_leaves_no_container() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-release");
    let graph = docker_graph("quick", "echo done");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let (driver, prefix) = docker_driver_named(graph, &dir, config).await;
    let report = driver.await_run().await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        report.releases.iter().any(|r| r.released_any("container")),
        "the release reported removing the container: {:?}",
        report.releases
    );

    // Only this test's own containers: the suite runs in parallel.
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// §6 driver test 3, Docker half: after a kill (a second cancel), the step goes
/// straight to `SIGKILL` — the 10s TERM grace is deliberately long enough that
/// waiting it out would fail the timing assertion — and release still leaves no
/// container behind.
#[tokio::test]
async fn docker_kill_leaves_no_container() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-kill");
    let graph = docker_graph(
        "stubborn",
        r#"
trap '' TERM
echo ready > ready
while :; do sleep 0.1; done
"#,
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(10))
        .with_cleanup_grace(Duration::from_secs(300))
        .with_retention(Retention::Never);
    let workspace = dir.workspace();

    let (driver, prefix) = docker_driver_named(graph, &dir, config).await;
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_file(&workspace.join("ready"), Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let killed_at = std::time::Instant::now();
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the kill ends the run")
        .expect("the run finished");
    let elapsed = killed_at.elapsed();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "stubborn").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "stubborn")["cancel_escalation"],
        json!("sigkill"),
        "the kill skipped the ladder"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "no TERM grace was waited out: {elapsed:?}"
    );
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// The fence half of the acquire contract (§9), across a driver's death: the
/// driver dies with a container step running, a fresh executor over the same
/// run dir acquires the same scope, and the crashed container stops mutating
/// the workspace — its name is rebuilt from the run id recorded in the run dir,
/// not re-minted.
#[tokio::test]
async fn a_new_executor_over_the_run_dir_fences_the_crashed_container() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-fence");
    // The beater runs in its own session: an aborted driver still lets the
    // orphaned step task stop its own process group as the channels close, so a
    // detached beater is what a dead *process* leaves behind — only a
    // container-level fence can end it. With `done` already in the workspace
    // the step exits at once, so the second run completes instead of beating.
    let graph = docker_graph(
        "beat",
        "[ -e done ] && exit 0\n\
         setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' &\n\
         sleep 300",
    );
    // The workspace is kept: release would otherwise remove the directory the
    // heartbeat is checked through, hiding a beater the fence missed.
    let config = || {
        RunConfig::new(dir.path())
            .with_grace(Duration::from_secs(2))
            .with_retention(Retention::Always)
    };
    let workspace = dir.workspace();
    let heartbeat = workspace.join("heartbeat");

    let (driver, prefix) = docker_driver_named(graph.clone(), &dir, config()).await;
    let run = tokio::spawn(driver.run());
    assert!(
        wait_for_file(&heartbeat, Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    // The crash: the driver is gone, release never runs, the container beats on.
    run.abort();
    let _ = run.await;
    assert!(
        !list_containers(&prefix).await.is_empty(),
        "the crashed run's container outlives its driver"
    );

    std::fs::write(workspace.join("done"), b"").expect("done");
    let report = docker_driver(graph, &dir, config()).await_run().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let before = file_len(&heartbeat);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        file_len(&heartbeat),
        before,
        "the crashed container kept writing: the fence missed it"
    );
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// A non-zero exit inside the container maps the same way it does on the host.
#[tokio::test]
async fn docker_exit_statuses_propagate() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-exit");
    let graph = docker_graph("failing", "echo about to fail; exit 7");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let report = docker_driver(graph, &dir, config).await_run().await;
    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "failing")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("exit_status:7"),
        "the exit status survived `docker exec`, `setsid` and the wrapper"
    );
}

/// A bad image is an acquire failure, not a run abort.
#[tokio::test]
async fn a_bad_image_fails_the_scope() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-bad-image");
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("petri-nonexistent/definitely-not-real:v0");
    let scope = b.add_scope(scope);
    b.add_node(
        "doomed",
        scope,
        StepRef::new(PROCESS_KIND, script("echo never runs")),
    );
    let graph = b.build();

    let config = RunConfig::new(dir.path()).with_retention(Retention::Never);
    let report = docker_driver(graph, &dir, config).await_run().await;

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "doomed")
        .expect("the firing failed rather than vanishing");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("env_acquire")
    );
    assert!(report.state.is_finished(), "the run completed");
}

/// The step's real duration and exit status both survive, whether or not `setsid`
/// forked. If `docker exec` returned early we would see a fast success here instead
/// of a slow failure.
#[tokio::test]
async fn docker_wait_follows_the_step_not_the_client() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-wait");
    let graph = docker_graph("slow-failure", "sleep 1; exit 5");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let started = std::time::Instant::now();
    let report = docker_driver(graph, &dir, config).await_run().await;
    let elapsed = started.elapsed();

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "slow-failure")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("exit_status:5")
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "the wait returned before the step was done: {elapsed:?}"
    );
}

/// The wrapper is inside the group the ladder kills, so it never gets to record a
/// status. That path must land on the cancellation outcome rather than hanging on a
/// status file that is not coming, or falling back to a default.
#[tokio::test]
async fn docker_cancel_without_a_recorded_status_still_reports_cancelled() {
    if !docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-killed-wrapper");
    // Ignores TERM, so the ladder escalates and SIGKILLs the whole group, wrapper
    // included, mid-step.
    let graph = docker_graph(
        "stubborn",
        r#"
trap '' TERM
echo ready > ready
while :; do sleep 0.1; done
"#,
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = docker_driver(graph, &dir, config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(60)).await);
    handle.cancel(ir::CancelScopeId::ROOT).await;

    let report = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the run must not hang waiting for a status file that is not coming")
        .expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "stubborn").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "stubborn")["cancel_escalation"],
        json!("sigkill"),
        "the escalation is recorded even though no status file was written"
    );

    // Nothing recorded a status, which is the state this test exists to cover.
    let leftovers: Vec<_> = std::fs::read_dir(workspace.join(".ci").join("pg"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !leftovers.iter().any(|n| n.ends_with(".status")),
        "a status file appeared after a SIGKILL: {leftovers:?}"
    );
}
