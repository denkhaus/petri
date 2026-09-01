//! Outcome-driven splice through the whole driver: the testkit `splice` step
//! kind drives uploads with no component, the known-secret backstop keeps a
//! splice payload out of the log, and the prefix rule reconstructs a batch
//! across a crash.

mod support;

use std::sync::Arc;

use driver::{Driver, RunConfig};
use engine::{Event, INVALID_SPLICE_CLASS};
use executor::{MapSecrets, Retention};
use ir::{
    Graph, GraphBuilder, GraphFragment, RunStatus, ScopeId, SplicePolicy, SpliceRequest, StepRef,
    Value,
};
use serde_json::json;
use steps::{PROCESS_KIND, Registry};
use support::*;

fn splice_runners() -> Registry {
    let mut registry = runners();
    registry.register(SpliceStep);
    registry
}

/// A fragment of chained process steps, each running `commands[i]`.
fn process_fragment(names_and_commands: &[(&str, &str)]) -> GraphFragment {
    GraphFragment::chain(
        names_and_commands
            .iter()
            .map(|(name, run)| (*name, StepRef::new(PROCESS_KIND, json!({ "run": run })))),
    )
}

/// `up` (splice step, Append) -> `down`; `up` uploads `requests`.
fn upload_graph(requests: &[SpliceRequest]) -> Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let up = b.add_node(
        "up",
        scope,
        StepRef::new(SPLICE_KIND, splice_config(requests, &json!("uploaded"))),
    );
    let down = add_script(&mut b, "down", scope, "echo downstream");
    b.link(up, down);
    b.node_mut(up).splice_policy = SplicePolicy::Append;
    b.build()
}

/// The testkit step kind uploads a real process fragment, the batch runs on the
/// host executor, and the dependent waits for it.
#[tokio::test]
async fn an_uploaded_fragment_runs_end_to_end() {
    let dir = RunDir::new("splice-e2e");
    let graph = upload_graph(&[SpliceRequest::append(process_fragment(&[
        ("gen-a", "echo spliced-a"),
        ("gen-b", "echo spliced-b"),
    ]))]);

    let report = host_driver_full(
        graph.clone(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
        splice_runners(),
    )
    .await_run()
    .await;

    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "gen-a").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "gen-b").as_deref(), Some("success"));
    let order = started(&report);
    let b_at = order.iter().position(|n| n == "gen-b").expect("gen-b ran");
    let down_at = order.iter().position(|n| n == "down").expect("down ran");
    assert!(b_at < down_at, "down waits for the batch: {order:?}");
    assert_eq!(report.state.splices().len(), 1);
    assert_replay_identical(&graph, &report);
}

/// A step that builds its request in code, with the secret value inside it —
/// the way a real generator would leak one. It cannot ride the node config:
/// the graph is recorded state, and the point is that nothing persisted may
/// carry the value.
struct LeakyUpload;

const LEAKY_KIND: ir::StepKindId = ir::StepKindId::new_static("leaky-upload");
const SECRET: &str = "sk-splice-4f9d2c81b7a3";

impl ir::StepKind for LeakyUpload {
    fn id(&self) -> ir::StepKindId {
        LEAKY_KIND
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the returned \
                  lifetime"
    )]
    fn name(&self) -> &str {
        "leaky-upload"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for LeakyUpload {
    async fn run(&self, _ctx: steps::StepCtx) -> ir::Outcome {
        let leak = format!("echo {SECRET}");
        ir::Outcome::success(Value::Null).with_splice(SpliceRequest::append(process_fragment(&[(
            "exfil",
            leak.as_str(),
        )])))
    }
}

/// The driver backstop: a registered secret value inside a splice request fails
/// the firing with no fragment applied — the request never reaches the log, and
/// nothing persisted carries the value.
#[tokio::test]
async fn a_secret_bearing_request_fails_the_firing_and_stays_out_of_the_log() {
    let dir = RunDir::new("splice-secret");

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    // The primer resolves the secret, which registers it with the masker: the
    // backstop is about *registered* values.
    let primer = b.add_node(
        "primer",
        scope,
        StepRef::new(
            PROCESS_KIND,
            json!({ "run": "true", "env": { "TOKEN": { "$secret": "TOKEN" } } }),
        ),
    );
    let up = b.add_node("up", scope, StepRef::new(LEAKY_KIND, json!({})));
    b.link(primer, up);
    b.node_mut(up).splice_policy = SplicePolicy::Append;
    let graph = b.build();
    let before = graph.nodes.len();

    let mut registry = splice_runners();
    registry.register_runner(Arc::new(LeakyUpload));
    let report = host_driver_full(
        graph.clone(),
        &dir,
        MapSecrets::from_pairs(&[("TOKEN", SECRET)]),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
        registry,
    )
    .await_run()
    .await;

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "up")
        .expect("up recorded");
    let info = record.outcome.status.failure_info().expect("a failure");
    assert_eq!(info.class, INVALID_SPLICE_CLASS);
    assert_eq!(
        report.state.graph().nodes.len(),
        before,
        "no fragment applied"
    );

    // The decisive check, matching the §11 shape: grep the serialized log and
    // the full state for the value.
    let log_bytes = serde_json::to_string(&report.state.log).expect("encode");
    assert!(!log_bytes.contains(SECRET), "the request reached the log");
    let state_bytes = serde_json::to_string(&report.state).expect("encode");
    assert!(
        !state_bytes.contains(SECRET),
        "the request reached the state"
    );
    assert_replay_identical(&graph, &report);
}

/// Resume mid-splice-lifecycle: the crash lands after the uploader's External
/// `StepFinished`, before the Core records it derived flush. The prefix rule
/// reconstructs the batch by replay, and the resumed run completes.
#[tokio::test]
async fn a_crash_between_the_finish_and_its_core_records_reconstructs_the_batch() {
    let dir = RunDir::new("splice-resume");
    let graph = upload_graph(&[SpliceRequest::append(process_fragment(&[(
        "gen-a",
        "echo spliced-a",
    )]))]);

    let report = host_driver_full(
        graph.clone(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
        splice_runners(),
    )
    .await_run()
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    // Cut immediately after the External finish: its Core derivations — the
    // routed tokens into the batch — are regenerated, not read back.
    let up_firing = firing_of(&report, "up");
    let cut = seq_of(
        &report.state.log,
        |r| matches!(&r.event, Event::StepFinished { firing, .. } if *firing == up_firing),
    ) + 1;
    let prefix = report.state.log.prefix(cut);

    let dir2 = RunDir::new("splice-resume-2");
    let executor: Arc<dyn executor::Executor> =
        Arc::new(executor_host::HostExecutor::new(dir2.path()));
    let (driver, info) = Driver::resume(
        graph.clone(),
        prefix,
        executor,
        splice_runners(),
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir2.path()).with_retention(Retention::Never),
    )
    .expect("the prefix resumes");
    assert!(
        info.redispatched.is_empty(),
        "routing must finish before the spliced step can be dispatched"
    );
    let resumed = driver.run().await;

    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    assert_eq!(status_of(&resumed, "gen-a").as_deref(), Some("success"));
    assert_eq!(resumed.state.splices().len(), 1, "the batch was rebuilt");
    assert_eq!(status_of(&resumed, "down").as_deref(), Some("success"));
}
