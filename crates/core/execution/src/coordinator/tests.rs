use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use driver::DeliverDisposition;
use ir::{Control, FiringId, GraphBuilder, Outcome, RunStatus, ScopeId, StepRef, Value};
use runtime::{RunOptions, Runtime};
use steps::{Step, StepCtx};
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::timeout;

use super::{Coordinator, CoordinatorError, CoordinatorOptions};
use crate::{
    CallSite, CoordinatorInvocationClient, ExecutionId, GraphDigest, InvocationClient as _,
    InvocationRequest, SandboxMode, SecretBindings, StoreError,
};

/// A run of another format is refused at resume by its run declaration,
/// before any record is decoded: the no-migration policy.
#[tokio::test]
async fn a_run_of_another_format_is_refused_at_resume() {
    let directory = RunDir::new("coordinator-old-format");
    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    drop(coordinator);
    let path = directory.path().join(crate::COORDINATOR_FILE);
    let text = fs::read_to_string(&path).unwrap();
    let mut lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["body"]["format_version"] = Value::from(1);
    let rewritten: Vec<String> = lines
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect();
    fs::write(&path, format!("{}\n", rewritten.join("\n"))).unwrap();
    let error = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .err()
    .expect("an old format is refused");
    assert!(
        matches!(
            error,
            CoordinatorError::Store(StoreError::UnsupportedFormat {
                found:    1,
                expected: crate::COORDINATOR_FORMAT_VERSION,
            })
        ),
        "{error}"
    );
}

#[derive(Default)]
pub(super) struct ReleaseGate {
    pub started:  Notify,
    pub complete: Notify,
}

#[derive(Default)]
struct Controls {
    delivered: Notify,
    cancelled: Notify,
}

struct WaitForChild;

#[async_trait::async_trait]
impl Step for WaitForChild {
    const NAME: &'static str = "test/wait-for-child";
    type Config = GraphDigest;

    async fn run(&self, graph: GraphDigest, mut ctx: StepCtx) -> Outcome {
        let controls = ctx.capability::<Arc<Controls>>().unwrap();
        let client = ctx.capability::<CoordinatorInvocationClient>().unwrap();
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
                admission: None,
            })
            .await
            .unwrap();
        loop {
            tokio::select! {
                _ = child.result() => panic!("child release must still be blocked"),
                control = ctx.control.recv() => match control {
                    Some(Control::Deliver(_)) => controls.delivered.notify_one(),
                    Some(Control::Cancel | Control::Kill) | None => {
                        controls.cancelled.notify_one();
                        return Outcome::cancelled();
                    }
                    Some(_) => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn child_lease_release_does_not_block_parent_control_or_cancellation() {
    let directory = RunDir::new("coordinator-release-controls");
    let controls = Arc::new(Controls::default());
    let runtime = Runtime::standard()
        .step(WaitForChild)
        .capability(controls.clone())
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let gate = Arc::new(ReleaseGate::default());
    coordinator.release_gate = Some(gate.clone());
    let mut child = GraphBuilder::new();
    child.add_step("child", ScopeId::new(0), "noop");
    let child = coordinator.register_graph(&child.build()).await.unwrap();
    let mut root = GraphBuilder::new();
    root.add_node(
        "parent",
        ScopeId::new(0),
        StepRef::new(WaitForChild::NAME, serde_json::json!(child)),
    );
    let root = coordinator.register_graph(&root.build()).await.unwrap();
    let handle = coordinator.handle();
    let (result, ()) = tokio::join!(coordinator.run_root(root, BTreeMap::new()), async {
        timeout(Duration::from_secs(10), gate.started.notified())
            .await
            .unwrap();
        assert_eq!(
            timeout(
                Duration::from_secs(1),
                handle.deliver(
                    ExecutionId::new(0),
                    FiringId::new(1),
                    Control::Deliver(Value::Null),
                )
            )
            .await
            .expect("control handling must not wait for provider release"),
            DeliverDisposition::Delivered
        );
        timeout(Duration::from_secs(1), controls.delivered.notified())
            .await
            .unwrap();
        handle.cancel_root();
        timeout(Duration::from_secs(1), controls.cancelled.notified())
            .await
            .expect("root cancellation must not wait for provider release");
        gate.complete.notify_one();
    },);
    assert_eq!(result.unwrap().status, RunStatus::Cancelled);
    coordinator.finish().await;
}
