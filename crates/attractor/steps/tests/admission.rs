//! Model resolution at admission: with a `PebbleClient` capability,
//! `Runtime::check` pins every agent and prompt node's route to concrete
//! `provider:model` pairs and freezes its fallback plan on the config; a
//! selector or a chain the catalog cannot resolve refuses the graph with a
//! named code on the node's span; without the capability the graph keeps
//! its selectors; and a graph admitted without a catalog still has its
//! `[run.model.fallbacks]` table checked by the `start` stage.

use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use attractor_steps::admission::{FALLBACKS_CODE, PLAN_KEY, UNKNOWN_CODE, resolve_graph};
use attractor_steps::fallback::{FrozenPlan, Route};
use attractor_steps::pebble::PebbleClient;
use attractor_steps::register;
use frontend::{CompileInputs, Diagnostics, NoFiles};
use frontend_attractor::fallbacks::CONFIG_KEY as FALLBACKS_KEY;
use frontend_attractor::{Attractor, RunSettings};
use ir::{Graph, RunStatus};
use lithos_llm::Client;
use pebble_coding_agent::test_support::{ScriptedCall, scripted_client, text_response};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};

/// One agent and one prompt node, on the scripted catalog's `test`
/// provider, the agent's model written as `provider/model`.
const WORKFLOW: &str = r#"digraph Admit {
    graph [backend="api", goal="Answer"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello.", model="test/model", provider="test"]
    tab [shape=tab, prompt="Summarize.", model="small", provider="test"]
    start -> agent -> tab -> exit
}"#;

fn client() -> Client {
    scripted_client(vec![ScriptedCall::response(text_response("ok"))]).0
}

fn chains(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(key, refs)| {
            (
                (*key).to_owned(),
                refs.iter().map(|r| (*r).to_owned()).collect(),
            )
        })
        .collect()
}

/// The bare language under `[run.model.fallbacks]`.
fn lower_with_chains(text: &str, chains: BTreeMap<String, Vec<String>>) -> Graph {
    let mut settings = RunSettings::default();
    settings.model.fallbacks = chains;
    let lowered = frontend_attractor::lower(
        "test.fabro",
        text,
        &NoFiles,
        &CompileInputs::new(),
        settings,
        Diagnostics::new(),
    );
    lowered
        .graph
        .unwrap_or_else(|| panic!("lowers: {:?}", lowered.diagnostics))
}

fn node<'a>(graph: &'a Graph, name: &str) -> &'a ir::Node {
    graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == name)
        .expect("the node")
}

fn runtime(dir: &RunDir, client: Option<Client>) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(100);
    options.retention = Retention::Never;
    let rt = register(Runtime::standard().frontend(Attractor::new())).options(options);
    match client {
        Some(client) => rt.capability(PebbleClient(client)),
        None => rt,
    }
}

fn check(dir: &RunDir, rt: &Runtime, text: &str) -> frontend::Lowered {
    let path = dir.path().join("wf.fabro");
    fs::write(&path, text).expect("the workflow");
    rt.check(&path, None, None, &CompileInputs::new())
        .expect("loads")
}

/// Every LLM node's selectors become the concrete route, its chain a
/// frozen plan with the notices, its `fallbacks` gone, its `meta` as
/// written; the `start` stage's table is gone too.
#[test]
fn check_pins_every_llm_node_and_freezes_its_plan() {
    let client = client();
    let mut graph = lower_with_chains(
        WORKFLOW,
        chains(&[
            ("model", &["test:small", "bare"]),
            ("small", &["test:vision"]),
        ]),
    );
    let problems = resolve_graph(&client, &mut graph);
    assert!(problems.is_empty(), "{problems:?}");

    let agent = node(&graph, "agent");
    let config = &agent.step.config;
    assert_eq!(config["model"], json!("model"));
    assert_eq!(config["provider"], json!("test"));
    assert!(config.get(FALLBACKS_KEY).is_none(), "{config}");
    let plan: FrozenPlan = serde_json::from_value(config[PLAN_KEY].clone()).expect("a plan");
    assert_eq!(plan.original.selector(), "test/model");
    assert_eq!(
        plan.remaining
            .iter()
            .map(Route::selector)
            .collect::<Vec<_>>(),
        ["test/small"]
    );
    // `bare` offers no `model`: skipped with Fabro's notice, which the
    // frozen plan carries for the stage's plan event.
    assert_eq!(plan.notices.len(), 1, "{:?}", plan.notices);
    assert_eq!(plan.notices[0].code, "model_fallback_skipped");
    assert_eq!(plan.notices[0].level, "warn");
    assert!(
        plan.notices[0].message.contains("`bare`"),
        "{}",
        plan.notices[0].message
    );
    assert_eq!(
        agent.meta["model"],
        json!("test/model"),
        "meta is the display record"
    );
    assert_eq!(agent.meta["provider"], json!("test"));

    let tab = node(&graph, "tab");
    assert_eq!(tab.step.config["model"], json!("small"));
    assert_eq!(tab.step.config["provider"], json!("test"));
    let plan: FrozenPlan =
        serde_json::from_value(tab.step.config[PLAN_KEY].clone()).expect("a plan");
    assert_eq!(plan.original.selector(), "test/small");
    assert_eq!(plan.remaining.len(), 1);
    assert_eq!(plan.remaining[0].selector(), "test/vision");
    // The table's notices ride every node's plan, as the stage carried
    // them; the run reports each once.
    assert_eq!(plan.notices.len(), 1, "{:?}", plan.notices);
    assert!(plan.notices[0].message.contains("`bare`"));

    let start = node(&graph, "start");
    assert!(
        start.step.config.get(FALLBACKS_KEY).is_none(),
        "{}",
        start.step.config
    );
}

/// `Runtime::check` runs the pass when the capability is installed, and
/// leaves the graph as the frontend lowered it when it is not.
#[test]
fn check_resolves_with_the_capability_and_not_without() {
    let dir = RunDir::new("admission-check");
    let lowered = check(&dir, &runtime(&dir, Some(client())), WORKFLOW);
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("admitted: {:?}", lowered.diagnostics));
    let config = &node(&graph, "agent").step.config;
    assert_eq!(config["model"], json!("model"));
    assert!(config.get(PLAN_KEY).is_some());

    let rt = runtime(&dir, None);
    let lowered = check(&dir, &rt, WORKFLOW);
    let graph = lowered.graph.expect("lowers");
    let config = &node(&graph, "agent").step.config;
    assert_eq!(config["model"], json!("test/model"));
    assert!(config.get(PLAN_KEY).is_none());
    let plain = rt
        .lower(
            &dir.path().join("wf.fabro"),
            None,
            None,
            &CompileInputs::new(),
        )
        .expect("loads");
    assert_eq!(
        serde_json::to_value(&graph).expect("json"),
        serde_json::to_value(plain.graph.expect("lowers")).expect("json"),
        "the graph is the frontend's"
    );
}

/// A model, a provider, or a provider-only node whose default the catalog
/// does not know refuses the graph with `attractor.model.unknown` at the
/// node's span.
#[test]
fn an_unknown_selector_refuses_the_graph_at_the_nodes_span() {
    let dir = RunDir::new("admission-unknown");
    let rt = runtime(&dir, Some(client()));
    let cases = [
        (
            "an unknown model",
            r#"digraph G {
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    a [prompt="x", model="nonesuch", provider="test"]
    start -> a -> exit
}"#,
        ),
        (
            "an unknown provider",
            r#"digraph G {
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    a [prompt="x", model="model", provider="nowhere"]
    start -> a -> exit
}"#,
        ),
        (
            "a provider-only prompt node on an unknown provider",
            r#"digraph G {
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    a [shape=tab, prompt="x", provider="nowhere"]
    start -> a -> exit
}"#,
        ),
    ];
    for (case, text) in cases {
        let lowered = check(&dir, &rt, text);
        assert!(lowered.graph.is_none(), "{case}: withheld");
        let errors: Vec<_> = lowered.diagnostics.errors().collect();
        assert_eq!(errors.len(), 1, "{case}: {errors:?}");
        assert_eq!(errors[0].code, UNKNOWN_CODE, "{case}");
        assert_eq!(errors[0].span.line, 5, "{case}: the node's line");
        assert!(
            errors[0].message.contains("`a`"),
            "{case}: {}",
            errors[0].message
        );
    }
}

/// A chain that names something the catalog does not know is
/// `attractor.model.unknown`; a chain that is malformed or keyed by a
/// provider is `attractor.model.fallbacks`. Both are the graph's, not a
/// node's, and are reported once however many nodes carry the table.
#[test]
fn a_bad_chain_refuses_the_graph_once_with_the_named_code() {
    let client = client();
    let cases = [
        (
            "key names a provider",
            chains(&[("test", &["bare"])]),
            FALLBACKS_CODE,
        ),
        (
            "keys conflict",
            chains(&[("model", &["test:small"]), ("test:model", &["test:vision"])]),
            FALLBACKS_CODE,
        ),
        (
            "reference does not parse",
            chains(&[("model", &["a/b/c"])]),
            FALLBACKS_CODE,
        ),
        (
            "key is unknown",
            chains(&[("nonesuch", &["test:small"])]),
            UNKNOWN_CODE,
        ),
        (
            "reference names an unknown provider",
            chains(&[("model", &["nowhere/small"])]),
            UNKNOWN_CODE,
        ),
    ];
    for (case, chains, code) in cases {
        let mut graph = lower_with_chains(WORKFLOW, chains);
        let problems = resolve_graph(&client, &mut graph);
        assert_eq!(problems.len(), 1, "{case}: {problems:?}");
        assert_eq!(problems[0].code, code, "{case}");
        assert_eq!(problems[0].node, None, "{case}: the table is the graph's");
        assert!(
            problems[0].message.contains("run.model.fallbacks"),
            "{case}: {}",
            problems[0].message
        );
    }
}

/// An ACP agent owns its model: the pass leaves it alone.
#[test]
fn an_acp_agent_is_left_alone() {
    let client = client();
    let mut graph = lower_with_chains(
        r#"digraph G {
    graph [backend="acp", acp.command="true"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    a [prompt="x"]
    start -> a -> exit
}"#,
        chains(&[("model", &["test:small"])]),
    );
    let before = serde_json::to_value(&node(&graph, "a").step.config).expect("json");
    let problems = resolve_graph(&client, &mut graph);
    assert!(problems.is_empty(), "{problems:?}");
    let after = serde_json::to_value(&node(&graph, "a").step.config).expect("json");
    assert_eq!(before, after);
}

/// A graph admitted without a catalog keeps its table on `start`, and the
/// stage checks it at run start as before: a key that names a provider
/// fails the run with class `bad_config` before any node runs.
#[tokio::test]
async fn the_start_stage_still_checks_the_table_when_admitted_without_a_catalog() {
    let dir = RunDir::new("admission-start-check");
    let graph = lower_with_chains(WORKFLOW, chains(&[("test", &["bare"])]));
    assert_eq!(
        node(&graph, "start").step.config[FALLBACKS_KEY],
        json!({ "test": ["bare"] })
    );
    let report = runtime(&dir, Some(client()))
        .run(graph)
        .await
        .expect("replay");
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status_of(&report, "start").as_deref(), Some("failure"));
    let output = output_of(&report, "start");
    assert_eq!(output["failure_class"], json!("bad_config"), "{output}");
    assert!(
        output["failure_reason"]
            .as_str()
            .is_some_and(|text| text.contains("names provider `test`")),
        "{output}"
    );
    assert!(
        status_of(&report, "agent").is_none(),
        "nothing ran after start"
    );
}
