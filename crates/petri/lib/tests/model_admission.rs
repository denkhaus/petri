//! Model resolution at admission through the embedding boundary: a run
//! admitted under one catalog runs on the routes admission pinned, however
//! the catalog changes before dispatch and again before resume, and a
//! pinned route the client can no longer address fails the stage with a
//! named class. The plan is read from the public events
//! (`attractor.fallback.plan`, and Pebble's `SessionStarted` route) and
//! from the scripted provider's request log.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use lithos_llm::Client;
use lithos_llm::adapter::ProviderAdapter;
use lithos_llm::catalog::Catalog;
use pebble_coding_agent::test_support::{ScriptedCall, ScriptedProvider, text_response};
use petri::attractor::admission::PLAN_KEY;
use petri::attractor::fallback::{PINNED_ROUTE_UNAVAILABLE_CLASS, PLAN_EVENT};
use petri::attractor::pebble::PebbleClient;
use petri::attractor::register;
use petri::engine::{EngineState, Event, EventRecord};
use petri::execution::controls::ControlService;
use petri::execution::events::{CollectingSink, EventProjector, RunEvent};
use petri::execution::host::{self, HostRun};
use petri::execution::{Access, CoordinatorRecord, ExecutionObserver};
use petri::executor::Retention;
use petri::frontend::CompileInputs;
use petri::frontend::fabro::Fabro;
use petri::ir::{ExecutionId, FiringId, Graph, Node, RunStatus, Value};
use petri::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, backend_event, output_of, status_of};
use tokio::sync::Notify;
use tokio::time::sleep;

/// Two agent stages on the alias `sol`, which every catalog below binds
/// differently.
const WORKFLOW: &str = r#"digraph Pinned {
    graph [backend="api", goal="Answer"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    a [prompt="First.", model="sol", on_failure="exit"]
    b [prompt="Second.", model="sol", on_failure="exit"]
    start -> a -> b -> exit
}"#;

/// The chain keyed by the model `sol` resolves to at admission.
const WORKFLOW_TOML: &str = "[run.model.fallbacks]\n\"model\" = [\"other:big\"]\n";

/// A catalog of two providers. `sol` is an alias of `test/model` when
/// `sol_on_test`, else of `other/big`; the provider with the higher
/// priority wins a bare selector.
fn catalog(sol_on_test: bool, test_priority: i32, other_priority: i32) -> Catalog {
    let (test_alias, other_alias) = if sol_on_test {
        ("aliases = [\"sol\"]", "")
    } else {
        ("", "aliases = [\"sol\"]")
    };
    let toml = format!(
        r#"
schema_version = 1

[providers.test]
display_name = "Test"
adapter = "test-adapter"
codec = "test-codec"
base_url = "http://127.0.0.1"
default_model = "model"
priority = {test_priority}

[providers.test.auth]
type = "none"

[providers.test.metadata.agent]
profile = "anthropic"

[providers.test.models.model]
display_name = "Test model"
api_model = "model"
{test_alias}
capabilities = {{ text = true, tools = true }}
limits = {{ context_tokens = 200000, max_output_tokens = 32000 }}

[providers.other]
display_name = "Other"
adapter = "test-adapter"
codec = "test-codec"
base_url = "http://127.0.0.2"
default_model = "big"
priority = {other_priority}

[providers.other.auth]
type = "none"

[providers.other.metadata.agent]
profile = "anthropic"

[providers.other.models.big]
display_name = "Big"
api_model = "big"
{other_alias}
capabilities = {{ text = true, tools = true }}
limits = {{ context_tokens = 200000, max_output_tokens = 32000 }}
"#
    );
    Catalog::builder()
        .overlay_toml(&toml)
        .expect("the catalog parses")
        .build()
        .expect("the catalog validates")
}

/// A scripted client over `catalog` with adapters for `enabled` alone.
fn client(
    catalog: Catalog,
    enabled: &[&str],
    calls: Vec<ScriptedCall>,
) -> (Client, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(calls));
    let shared: Arc<dyn ProviderAdapter> = provider.clone();
    let mut builder = Client::builder()
        .catalog(catalog)
        .enabled_providers(enabled.iter().map(|p| (*p).to_owned()));
    for id in enabled {
        builder = builder.adapter_arc(*id, shared.clone());
    }
    let build = builder.build().expect("the client builds");
    assert!(build.issues.is_empty(), "{:?}", build.issues);
    (build.client, provider)
}

/// Catalog A, the one the run is admitted under: `sol` is `test/model`,
/// `test` outranks `other`, both are available.
fn admitting_client() -> Client {
    client(catalog(true, 10, 0), &["test", "other"], Vec::new()).0
}

fn runtime(dir: &RunDir, client: Client, controls: Option<&ControlService>) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Never;
    options.echo = false;
    let mut rt = Runtime::standard().frontend(Fabro::new());
    if let Some(controls) = controls {
        rt = rt.hooks(controls.hooks(None));
    }
    register(rt)
        .capability(PebbleClient(client))
        .options(options)
}

/// Admit the workflow under catalog A.
fn admit(dir: &RunDir) -> Graph {
    let rt = runtime(dir, admitting_client(), None);
    fs::write(dir.path().join("wf.fabro"), WORKFLOW).expect("workflow");
    fs::write(dir.path().join("workflow.toml"), WORKFLOW_TOML).expect("workflow.toml");
    let lowered = rt
        .check(
            &dir.path().join("wf.fabro"),
            None,
            None,
            &CompileInputs::new(),
        )
        .expect("loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("admitted: {:?}", lowered.diagnostics));
    for name in ["a", "b"] {
        let config = &node(&graph, name).step.config;
        assert_eq!(config["model"], json!("model"), "{name}");
        assert_eq!(config["provider"], json!("test"), "{name}");
        assert_eq!(config[PLAN_KEY]["original"]["provider"], json!("test"));
        assert_eq!(config[PLAN_KEY]["remaining"][0]["provider"], json!("other"));
        assert_eq!(config[PLAN_KEY]["remaining"][0]["model"], json!("big"));
    }
    graph
}

fn node<'a>(graph: &'a Graph, name: &str) -> &'a Node {
    graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == name)
        .expect("the node")
}

/// What the public events say a stage ran on: the plan's routes and the
/// route Pebble's session started on.
#[derive(Debug, Default, PartialEq, Eq)]
struct Ran {
    plan:    Vec<String>,
    session: Vec<String>,
}

fn route_of(value: &Value) -> String {
    format!(
        "{}/{}",
        value["provider"].as_str().unwrap_or(""),
        value["model"].as_str().unwrap_or("")
    )
}

fn ran(events: &[RunEvent], node: &str) -> Ran {
    let mut out = Ran::default();
    for event in events {
        let Some(value) = event.custom() else {
            continue;
        };
        if value["kind"] == PLAN_EVENT && value["node"] == node {
            out.plan = value["routes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(route_of)
                .collect();
        }
        if let Some(activity) = backend_event(value)
            && activity.backend == "pebble"
            && activity.parent_session.is_none()
            && event
                .subject
                .as_ref()
                .is_some_and(|subject| subject.node.name == node)
            && let Some(payload) = activity.envelope["event"].get("SessionStarted")
        {
            out.session.push(route_of(payload));
        }
    }
    out
}

/// Pause the run the moment `node` starts, and say when it has finished.
struct PauseOnStart {
    node:     &'static str,
    controls: ControlService,
    finished: Arc<Notify>,
}

impl ExecutionObserver for PauseOnStart {
    fn on_engine_record(
        &self,
        _: ExecutionId,
        record: &EventRecord,
        _recorded_at: u64,
        state: &EngineState,
    ) {
        let named = |firing: &FiringId| {
            state
                .firing_node(*firing)
                .and_then(|id| state.graph().node(id))
                .is_some_and(|node| node.name == self.node)
        };
        match &record.event {
            Event::StepStarted { firing, .. } if named(firing) => self.controls.pause(),
            Event::StepFinished { firing, .. } if named(firing) => self.finished.notify_one(),
            _ => {}
        }
    }

    fn on_lifecycle(&self, _: &CoordinatorRecord) {}
}

/// Admitted under A; dispatched under B, where `sol` is `other/big`,
/// `other` outranks `test` and `test`'s default moved; paused before `b`
/// and dropped; resumed under C, where `other` is no longer available.
/// Both stages run on `test/model`, the admitted route, with the admitted
/// chain, and the stored root graph carries the frozen plans.
#[tokio::test]
async fn the_admitted_routes_survive_catalog_changes_before_dispatch_and_before_resume() {
    let dir = RunDir::new("model-admission-pinned");
    let graph = admit(&dir);

    // Catalog B for dispatch.
    let (client_b, provider_b) = client(catalog(false, 0, 20), &["test", "other"], vec![
        ScriptedCall::response(text_response("First answer.")),
    ]);
    let controls = ControlService::new();
    let rt_b = runtime(&dir, client_b, Some(&controls));
    let sink_b = Arc::new(CollectingSink::default());
    let projector_b = EventProjector::new(sink_b.clone());
    let finished = Arc::new(Notify::new());
    let host_run = HostRun::new(graph)
        .observe(Arc::new(controls.clone()))
        .observe(Arc::new(PauseOnStart {
            node:     "a",
            controls: controls.clone(),
            finished: finished.clone(),
        }))
        .observe(projector_b.clone());
    let wired = controls.clone();
    let run = Box::pin(host::run_configured(&rt_b, host_run, |handle, _| {
        wired.wire(handle);
    }));
    tokio::select! {
        report = run => panic!(
            "the run finished while paused: {:?}",
            report.map(|report| (
                report.status,
                report
                    .state
                    .history()
                    .iter()
                    .map(|row| format!("{} {} {}", row.name, row.outcome.status.tag(), row.outcome.output))
                    .collect::<Vec<_>>()
            ))
        ),
        () = async {
            finished.notified().await;
            // `b` reaches admission and is held; the pause record is durable.
            sleep(Duration::from_millis(400)).await;
        } => {}
    }
    assert!(controls.is_paused());
    let _ = projector_b.shutdown().await;
    let requested: Vec<String> = provider_b
        .requests()
        .iter()
        .map(|request| request.model().to_owned())
        .collect();
    assert_eq!(requested, ["test/model"], "`a` ran on the admitted route");
    assert_eq!(ran(&sink_b.events(), "a"), Ran {
        plan:    vec!["test/model".into(), "other/big".into()],
        session: vec!["test/model".into()],
    });

    // The persisted graph is the admitted one.
    let rt_read = runtime(&dir, admitting_client(), None);
    let logs = rt_read
        .open_run(dir.path(), Access::Read)
        .await
        .expect("opens");
    let stored = host::stored_root_graph(&*logs)
        .await
        .expect("reads")
        .expect("a root graph");
    assert_eq!(
        node(&stored, "b").step.config[PLAN_KEY]["original"],
        json!({ "provider": "test", "model": "model" })
    );
    drop(logs);

    // Catalog C for the resume: `other` is gone from the available set and
    // `sol` still names it.
    let (client_c, provider_c) = client(catalog(false, 0, 20), &["test"], vec![
        ScriptedCall::response(text_response("Second answer.")),
    ]);
    let controls = ControlService::new();
    let rt_c = runtime(&dir, client_c, Some(&controls));
    let sink_c = Arc::new(CollectingSink::default());
    let projector_c = EventProjector::new(sink_c.clone());
    let releaser = controls.clone();
    let report = host::resume_configured(
        &rt_c,
        Vec::new(),
        vec![Arc::new(controls.clone()), projector_c.clone()],
        |handle, _| {
            releaser.wire(handle);
            tokio::spawn(async move {
                sleep(Duration::from_millis(500)).await;
                releaser.unpause().await;
            });
        },
    )
    .await
    .expect("resumes");
    let _ = projector_c.shutdown().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requested: Vec<String> = provider_c
        .requests()
        .iter()
        .map(|request| request.model().to_owned())
        .collect();
    assert_eq!(requested, ["test/model"], "`b` ran on the admitted route");
    assert_eq!(ran(&sink_c.events(), "b"), Ran {
        plan:    vec!["test/model".into(), "other/big".into()],
        session: vec!["test/model".into()],
    });
    assert_eq!(output_of(&report, "b")["text"], json!("Second answer."));
}

/// The admitted route's provider is not available at dispatch: the stage
/// fails with `llm:pinned_route_unavailable`, naming the route, and no
/// request leaves.
#[tokio::test]
async fn a_pinned_route_the_client_cannot_address_fails_the_stage_with_a_named_class() {
    let dir = RunDir::new("model-admission-unavailable");
    let graph = admit(&dir);
    let (client_d, provider_d) = client(catalog(false, 0, 20), &["other"], vec![
        ScriptedCall::response(text_response("Never sent.")),
    ]);
    let rt_d = runtime(&dir, client_d, None);
    let report = host::run(&rt_d, graph).await.expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    let output = output_of(&report, "a");
    assert_eq!(
        output["failure_class"],
        json!(PINNED_ROUTE_UNAVAILABLE_CLASS),
        "{output}"
    );
    assert!(
        output["failure_reason"]
            .as_str()
            .is_some_and(|text| text.contains("`test/model`")),
        "{output}"
    );
    assert!(provider_d.requests().is_empty(), "no request left");
    assert!(status_of(&report, "b").is_none(), "`b` never ran");
}
