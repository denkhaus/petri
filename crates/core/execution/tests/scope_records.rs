//! The scope records: where each scope's environment ran (`scope.acquired`),
//! why it could not be acquired (`scope.failed`), and what became of its
//! sandbox once the invocation that owned it finished (`scope.released`).
//! Proved from the public stream alone, live and replayed, on the host
//! sandbox and on Docker.

use engine::Event;
use execution::events::{RunEvent, replay_run_dir, verify_export_run_dir};
use execution::prune::prune;
use execution::{CoordinatorEvent, HOST_PROVIDER, InvocationId, host};
use executor::{Retention, ScopeOutcome};
use ir::{GraphBuilder, RunStatus, RuntimeSpec, Scope, ScopeId, ServiceSpec, WorkspaceId};
use runtime::steps::PROCESS_KIND;
use runtime::{RunOptions, Runtime};
use testkit::{RunDir, add_script, is_docker_ready, script_with};

/// One script in one scope of `runtime`, under `sh`: the container image
/// carries no bash.
fn one_step(runtime: RuntimeSpec, run: &str) -> ir::Graph {
    let mut builder = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = runtime;
    let scope = builder.add_scope(scope);
    builder.add_node(
        "work",
        scope,
        ir::StepRef::new(
            PROCESS_KIND,
            script_with(run, &serde_json::json!({ "shell": "sh" })),
        ),
    );
    builder.build()
}

/// The stream's `scope.acquired` records, as (engine record, event).
fn acquired(events: &[RunEvent]) -> Vec<&RunEvent> {
    events
        .iter()
        .filter(|event| matches!(event.engine(), Some(Event::ScopeAcquired { .. })))
        .collect()
}

fn failed(events: &[RunEvent]) -> Vec<&RunEvent> {
    events
        .iter()
        .filter(|event| matches!(event.engine(), Some(Event::ScopeFailed { .. })))
        .collect()
}

fn released(events: &[RunEvent]) -> Vec<&RunEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.coordinator(),
                Some(CoordinatorEvent::ScopeReleased { .. })
            )
        })
        .collect()
}

/// Every failure message the run's firings reported, for a diagnosis.
fn failures(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event.engine() {
            Some(Event::StepFinished { outcome, .. }) => outcome
                .status
                .failure_info()
                .map(|info| info.message.clone()),
            _ => None,
        })
        .collect()
}

fn position(events: &[RunEvent], find: impl Fn(&RunEvent) -> bool) -> usize {
    events
        .iter()
        .position(find)
        .expect("the stream carries the record")
}

struct Recorded {
    dir:    RunDir,
    rt:     Runtime,
    status: RunStatus,
    events: Vec<RunEvent>,
}

async fn run_recorded(label: &str, graph: ir::Graph, retention: Retention) -> Recorded {
    let dir = RunDir::new(label);
    let mut options = RunOptions::new(dir.path());
    options.retention = retention;
    let rt = Runtime::standard().options(options);
    let report = host::run(&rt, graph).await.expect("the run completes");
    verify_export_run_dir(dir.path())
        .await
        .expect("the stream exports its logs, scope records included");
    let events = replay_run_dir(dir.path()).await.expect("the run replays");
    Recorded {
        dir,
        rt,
        status: report.status,
        events,
    }
}

/// The one `scope.acquired` of a run: the scope, the lease and workspace the
/// coordinator gave it, the sandbox the executor acquired, and the time it
/// took, before any attempt ran in the scope.
async fn check_acquired(
    label: &str,
    runtime: RuntimeSpec,
    provider: &str,
    retention: Retention,
    retained: bool,
) {
    let Recorded {
        dir,
        rt,
        status,
        events,
    } = run_recorded(label, one_step(runtime, "echo ran"), retention).await;
    assert_eq!(status, RunStatus::Success, "{:?}", failures(&events));

    let [acquired_event] = acquired(&events)[..] else {
        panic!("one scope, one acquisition: {events:#?}");
    };
    assert!(failed(&events).is_empty());
    let Some(Event::ScopeAcquired {
        scope,
        lease,
        workspace,
        sandbox,
        duration_ms: _,
    }) = acquired_event.engine()
    else {
        unreachable!()
    };
    assert_eq!(*scope, ScopeId::new(0));
    assert_eq!(lease.map(|lease| lease.raw()), Some(0));
    assert_eq!(
        *workspace,
        WorkspaceId::scoped(
            Some(&InvocationId::ROOT.workspace_prefix()),
            ScopeId::new(0)
        )
    );
    assert_eq!(sandbox.provider, provider);
    assert!(
        !sandbox.instance.is_empty(),
        "the provider's id: {sandbox:?}"
    );
    assert!(
        !sandbox.working_directory.is_empty(),
        "the directory steps run in: {sandbox:?}"
    );
    assert_eq!(
        acquired_event.context.execution,
        Some(ir::ExecutionId::new(0))
    );
    assert!(acquired_event.subject.is_none(), "a scope is not a node");
    assert!(
        acquired_event.derived.is_none(),
        "nothing is derived beside the record"
    );
    if provider == HOST_PROVIDER {
        // The provider reports the canonical path; a deleted workspace can
        // no longer be canonicalized itself, so the run dir is.
        let expected = std::fs::canonicalize(dir.path())
            .expect("the run dir")
            .join("scopes")
            .join(workspace.as_str())
            .join("work");
        assert_eq!(
            sandbox.working_directory.as_str(),
            expected.to_string_lossy(),
            "the host workspace is the run dir's"
        );
        assert!(sandbox.image.is_none());
    }

    // Before the first attempt in the scope; the release before the run's end.
    let first_start = position(&events, |event| {
        matches!(event.engine(), Some(Event::StepStarted { .. }))
    });
    let acquired_at = position(&events, |event| std::ptr::eq(event, acquired_event));
    assert!(
        acquired_at < first_start,
        "acquired before any attempt runs"
    );

    let [released_event] = released(&events)[..] else {
        panic!("one lease, one release: {events:#?}");
    };
    let Some(CoordinatorEvent::ScopeReleased {
        invocation,
        lease: released_lease,
        scope: identity,
        workspace: released_workspace,
        provider: released_provider,
        instance,
        outcome,
        retained: was_retained,
        problems,
    }) = released_event.coordinator()
    else {
        unreachable!()
    };
    assert_eq!(*invocation, InvocationId::ROOT);
    assert_eq!(Some(*released_lease), *lease);
    assert_eq!(*identity, engine::ScopeIdentity::Declared(ScopeId::new(0)));
    assert_eq!(released_workspace, workspace);
    assert_eq!(released_provider, provider);
    assert_eq!(instance.as_ref(), Some(&sandbox.instance));
    assert_eq!(*outcome, ScopeOutcome::Succeeded);
    assert_eq!(*was_retained, retained, "retention {retention:?}");
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(released_event.context.invocation, Some(InvocationId::ROOT));
    let run_finished = position(&events, |event| {
        matches!(
            event.coordinator(),
            Some(CoordinatorEvent::RunFinished { .. })
        )
    });
    let released_at = position(&events, |event| std::ptr::eq(event, released_event));
    assert!(
        released_at < run_finished,
        "released before the run finishes"
    );

    let record = execution::ResourceStore::load(&testkit::read_run_dir(dir.path()).await)
        .await
        .expect("resources")
        .records()
        .next()
        .cloned()
        .expect("the lease's record");
    assert_eq!(
        record.state == execution::LeaseState::Deleted,
        !retained,
        "the record agrees with the release"
    );
    // A retained sandbox outlives the test unless pruned.
    let report = prune(&rt).await.expect("the retained sandbox prunes");
    assert!(report.is_clean(), "{report:?}");
}

#[tokio::test]
async fn a_host_scope_records_its_sandbox_and_its_retained_release() {
    check_acquired(
        "scope-records-host-retained",
        RuntimeSpec::default(),
        HOST_PROVIDER,
        Retention::Always,
        true,
    )
    .await;
}

#[tokio::test]
async fn a_host_scope_records_its_deleted_release() {
    check_acquired(
        "scope-records-host-deleted",
        RuntimeSpec::default(),
        HOST_PROVIDER,
        Retention::Never,
        false,
    )
    .await;
}

#[tokio::test]
async fn a_docker_scope_records_its_container_and_image() {
    if !is_docker_ready().await {
        return;
    }
    let Recorded { status, events, .. } = run_recorded(
        "scope-records-docker",
        one_step(RuntimeSpec::container("alpine:3.20"), "echo ran"),
        Retention::Never,
    )
    .await;
    assert_eq!(status, RunStatus::Success, "{:?}", failures(&events));
    let [acquired_event] = acquired(&events)[..] else {
        panic!("one scope, one acquisition: {events:#?}");
    };
    let Some(Event::ScopeAcquired { sandbox, .. }) = acquired_event.engine() else {
        unreachable!()
    };
    assert_eq!(sandbox.provider, "docker");
    assert_eq!(sandbox.image.as_deref(), Some("alpine:3.20"));
    assert_eq!(sandbox.working_directory, "/workspace");
    assert!(!sandbox.instance.is_empty());
    let [released_event] = released(&events)[..] else {
        panic!("one lease, one release: {events:#?}");
    };
    let Some(CoordinatorEvent::ScopeReleased {
        provider,
        instance,
        retained,
        problems,
        ..
    }) = released_event.coordinator()
    else {
        unreachable!()
    };
    assert_eq!(provider, "docker");
    assert_eq!(instance.as_ref(), Some(&sandbox.instance));
    assert!(!retained, "Retention::Never deletes the container");
    assert!(problems.is_empty(), "{problems:?}");
}

#[tokio::test]
async fn a_docker_scope_kept_by_retention_records_it_as_retained() {
    if !is_docker_ready().await {
        return;
    }
    check_acquired(
        "scope-records-docker-retained",
        RuntimeSpec::container("alpine:3.20"),
        "docker",
        Retention::Always,
        true,
    )
    .await;
}

/// A scope the executor refuses: a host-process scope with a sidecar. The
/// record carries the error and its causes, every firing in the scope fails
/// with the same message, and no sandbox exists to release.
#[tokio::test]
async fn a_scope_that_cannot_be_acquired_records_the_failure() {
    let mut builder = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.services.push(ServiceSpec::new("db", "postgres:16"));
    let scope = builder.add_scope(scope);
    add_script(&mut builder, "work", scope, "echo never");
    let Recorded { status, events, .. } =
        run_recorded("scope-records-failed", builder.build(), Retention::Always).await;
    assert_eq!(status, RunStatus::Failed);

    assert!(acquired(&events).is_empty());
    let [failed_event] = failed(&events)[..] else {
        panic!("one scope, one failed acquisition: {events:#?}");
    };
    let Some(Event::ScopeFailed {
        scope,
        lease,
        workspace,
        provider,
        error,
        causes,
        duration_ms: _,
    }) = failed_event.engine()
    else {
        unreachable!()
    };
    assert_eq!(*scope, ScopeId::new(0));
    assert_eq!(lease.map(|lease| lease.raw()), Some(0));
    assert_eq!(
        *workspace,
        WorkspaceId::scoped(
            Some(&InvocationId::ROOT.workspace_prefix()),
            ScopeId::new(0)
        )
    );
    assert_eq!(provider.as_deref(), Some(HOST_PROVIDER));
    assert!(
        error.contains("services require a containerized job"),
        "the executor's refusal: {error}"
    );
    assert!(failed_event.subject.is_none());

    // The firing's failure carries the same rendered chain.
    let outcome = events
        .iter()
        .find_map(|event| match event.engine() {
            Some(Event::StepFinished { outcome, .. }) => Some(outcome.clone()),
            _ => None,
        })
        .expect("the firing finished");
    let message = outcome
        .status
        .failure_info()
        .map(|info| info.message.clone())
        .expect("a failure");
    let mut rendered = error.clone();
    for cause in causes {
        rendered.push_str(": ");
        rendered.push_str(cause);
    }
    assert_eq!(
        message,
        format!("could not acquire the environment: {rendered}")
    );

    // The lease was reserved on the provider before the refusal, so the
    // run's end still releases it: nothing existed, nothing is retained.
    let [released_event] = released(&events)[..] else {
        panic!("one lease, one release: {events:#?}");
    };
    let Some(CoordinatorEvent::ScopeReleased {
        lease: released_lease,
        instance,
        outcome,
        retained,
        problems,
        ..
    }) = released_event.coordinator()
    else {
        unreachable!()
    };
    assert_eq!(Some(*released_lease), *lease);
    assert!(instance.is_none(), "no sandbox was created: {instance:?}");
    assert_eq!(*outcome, ScopeOutcome::Failed);
    assert!(!retained);
    assert!(problems.is_empty(), "{problems:?}");
}
