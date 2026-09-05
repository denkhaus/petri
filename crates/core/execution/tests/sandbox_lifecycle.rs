use std::collections::BTreeMap;
use std::fs;

use execution::prune::prune;
use execution::{
    CallSite, Coordinator, CoordinatorInvocationClient, CoordinatorOptions, GraphDigest,
    InvocationClient as _, InvocationRequest, SandboxMode, SecretBindings,
};
use executor::{OneShotContainer, Retention};
use ir::{GraphBuilder, Outcome, RunStatus, RuntimeSpec, Scope, ScopeId, StepRef};
use runtime::steps::{Step, StepCtx};
use runtime::{RunOptions, Runtime};
use testkit::{RunDir, container_id, container_is_running, is_docker_ready, sandbox_name};

struct HandleChildFailure;

#[async_trait::async_trait]
impl Step for HandleChildFailure {
    const NAME: &'static str = "test/handle-child-failure";
    type Config = GraphDigest;

    async fn run(&self, graph: GraphDigest, ctx: StepCtx) -> Outcome {
        let client = ctx
            .require_capability::<CoordinatorInvocationClient>()
            .expect("coordinator client");
        let mut child = client
            .start_or_attach(InvocationRequest {
                site: CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    "child".into(),
                },
                graph,
                context: BTreeMap::new(),
                secrets: SecretBindings::None,
                sandbox: SandboxMode::Isolated,
            })
            .await
            .expect("child starts");
        assert_eq!(child.result().await.status, RunStatus::Failed);
        Outcome::success(serde_json::Value::Null)
    }
}

#[tokio::test]
async fn successful_run_keeps_a_failed_childs_retained_sandbox() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("coordinator-child-retention");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::OnFailure;
    let runtime = Runtime::standard()
        .step(HandleChildFailure)
        .options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("coordinator starts");
    let mut child = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("alpine:3.20");
    let scope = child.add_scope(scope);
    child.add_node(
        "fail",
        scope,
        StepRef::new(
            "process",
            serde_json::json!({ "run": "exit 1", "shell": "sh" }),
        ),
    );
    let child = coordinator
        .register_graph(&child.build())
        .expect("child graph");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "handle-failure",
        ScopeId::new(0),
        StepRef::new(HandleChildFailure::NAME, serde_json::json!(child)),
    );
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent graph");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("run");
    assert_eq!(result.status, RunStatus::Success);
    let sandbox = sandbox_name(directory.path(), 1);
    let retained = container_id(&sandbox)
        .await
        .expect("child sandbox retained");
    coordinator.finish().await;
    assert_eq!(
        container_id(&sandbox).await.as_deref(),
        Some(retained.as_str())
    );
    assert!(!container_is_running(&sandbox).await);
    let report = prune(&runtime).await.expect("cleanup retained sandbox");
    assert!(report.is_clean(), "{report:?}");
}

struct HostAction;

#[async_trait::async_trait]
impl Step for HostAction {
    const NAME: &'static str = "test/host-action";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        let runner = ctx.require_container_runner().expect("host runner");
        let mut process = runner
            .run(OneShotContainer::registry("alpine:3.20").with_args(&["true"]))
            .await
            .expect("action starts");
        let mut lines = process.lines().expect("action output");
        while lines.recv().await.is_some() {}
        assert!(process.wait().await.expect("action finishes").is_success());
        Outcome::success(serde_json::Value::Null)
    }
}

#[tokio::test]
async fn invocation_finish_releases_its_host_action_sandbox() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("coordinator-host-action-release");
    let runtime = Runtime::standard()
        .step(HostAction)
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("coordinator starts");
    let mut graph = GraphBuilder::new();
    graph.add_step("action", ScopeId::new(0), HostAction::NAME);
    let graph = coordinator.register_graph(&graph.build()).expect("graph");
    let result = coordinator
        .run_root(graph, BTreeMap::new())
        .await
        .expect("run");
    assert_eq!(result.status, RunStatus::Success);
    let prefix = format!("petri-{}-", testkit::recorded_run_id(directory.path()));
    assert!(testkit::list_containers(&prefix).await.is_empty());
    coordinator.finish().await;
}

#[tokio::test]
async fn an_execution_restart_keeps_the_container_identity_and_workspace() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("coordinator-container-restart");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard().options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let mut graph = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("alpine:3.20");
    let scope = graph.add_scope(scope);
    let start = graph.add_node(
        "before",
        scope,
        StepRef::new(
            "process",
            serde_json::json!({
                "run": "printf '%s' \"$HOSTNAME\" > before", "shell": "sh",
            }),
        ),
    );
    let after = graph.add_node(
        "after",
        scope,
        StepRef::new(
            "process",
            serde_json::json!({
                "run": "test \"$(cat before)\" = \"$HOSTNAME\"", "shell": "sh",
            }),
        ),
    );
    graph.mark_entry(start);
    graph.link(start, after);
    graph.node_mut(start).routing.groups[0].arms[0].transition = ir::EdgeTransition::Restart;
    let graph = coordinator.register_graph(&graph.build()).unwrap();
    let result = coordinator.run_root(graph, BTreeMap::new()).await.unwrap();
    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(result.final_execution.raw(), 1);
    let records = execution::ResourceStore::load(directory.path().join("resources")).unwrap();
    assert_eq!(
        records.records().count(),
        1,
        "both executions share one lease"
    );
    coordinator.finish().await;
    assert!(prune(&runtime).await.unwrap().is_clean());
}

#[tokio::test]
async fn a_failed_host_scope_is_retained_and_pruned_through_a_fresh_plugin() {
    let directory = RunDir::new("host-retention-prune");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::OnFailure;
    let runtime = Runtime::standard().options(options.clone());
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("coordinator");
    let mut graph = GraphBuilder::new();
    graph.add_node(
        "fail",
        ScopeId::new(0),
        StepRef::new(
            "process",
            serde_json::json!({ "run": "echo retained > proof; exit 1", "shell": "sh" }),
        ),
    );
    let graph = coordinator.register_graph(&graph.build()).expect("graph");
    let result = coordinator
        .run_root(graph, BTreeMap::new())
        .await
        .expect("run");
    coordinator.finish().await;
    assert_eq!(result.status, RunStatus::Failed);
    let store = execution::ResourceStore::load(directory.path().join(execution::RESOURCES_DIR))
        .expect("resources");
    let record = store.records().next().expect("host lease");
    assert_eq!(record.provider.as_str(), "host");
    assert_eq!(record.state, execution::LeaseState::Stopped);
    assert!(
        record
            .resource_id
            .as_ref()
            .expect("provider id")
            .starts_with("host-")
    );
    let workspace = directory
        .path()
        .join("scopes")
        .join(record.workspace.as_str())
        .join("work");
    assert_eq!(
        fs::read_to_string(workspace.join("proof")).expect("retained proof"),
        "retained\n"
    );
    let fresh = Runtime::standard().options(options);
    let report = prune(&fresh)
        .await
        .expect("prune through a new Host plugin");
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.deleted.len(), 1);
    assert!(
        !workspace.exists(),
        "provider deletion removes its managed workspace"
    );
    let store = execution::ResourceStore::load(directory.path().join(execution::RESOURCES_DIR))
        .expect("resources");
    assert_eq!(
        store.records().next().expect("tombstone").state,
        execution::LeaseState::Deleted
    );
}
