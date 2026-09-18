//! The in-memory check entry point through the embedding boundary: a host
//! that holds a workflow bundle as a map of paths to text (Fabro keeps a
//! workflow version that way) checks it with `Runtime::check_source` and
//! gets the graph `Runtime::check` produces from the same files on disk,
//! with the settings files beside the workflow and at the root read from
//! the map, every diagnostic naming the in-memory path, and the admission
//! passes run.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use lithos_llm::Client;
use lithos_llm::adapter::ProviderAdapter;
use lithos_llm::catalog::Catalog;
use pebble_coding_agent::test_support::ScriptedProvider;
use petri::attractor::admission::{PLAN_KEY, UNKNOWN_CODE};
use petri::attractor::pebble::PebbleClient;
use petri::attractor::register;
use petri::frontend::fabro::Fabro;
use petri::frontend::{CompileInputs, Lowered, MapFiles, REPOSITORY_VAR};
use petri::ir::{Graph, Node};
use petri::{RunOptions, Runtime};
use serde_json::json;
use testkit::RunDir;

/// The workflow, one directory below the root. Its stage names no model:
/// the project layer supplies it.
const WORKFLOW_FILE: &str = "flows/wf.fabro";

const WORKFLOW: &str = r#"digraph Bundle {
    graph [backend="api", goal="Answer"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    a [prompt="First.", on_failure="exit"]
    start -> a -> exit
}"#;

/// Beside the workflow: the chain the model `model` falls back on.
const WORKFLOW_TOML: &str = "[run.model.fallbacks]\n\"model\" = [\"test:big\"]\n";

/// At the root: the model every stage runs on unless it names its own.
const PROJECT_TOML: &str = "[run.model]\nname = \"sol\"\n";

/// A catalog of one provider with two models; `sol` is an alias of `model`.
fn catalog() -> Catalog {
    let toml = r#"
schema_version = 1

[providers.test]
display_name = "Test"
adapter = "test-adapter"
codec = "test-codec"
base_url = "http://127.0.0.1"
default_model = "model"

[providers.test.auth]
type = "none"

[providers.test.metadata.agent]
profile = "anthropic"

[providers.test.models.model]
display_name = "Test model"
api_model = "model"
aliases = ["sol"]
capabilities = { text = true, tools = true }
limits = { context_tokens = 200000, max_output_tokens = 32000 }

[providers.test.models.big]
display_name = "Big"
api_model = "big"
capabilities = { text = true, tools = true }
limits = { context_tokens = 200000, max_output_tokens = 32000 }
"#;
    Catalog::builder()
        .overlay_toml(toml)
        .expect("the catalog parses")
        .build()
        .expect("the catalog validates")
}

/// A scripted client over the catalog: enough for admission to resolve
/// every route; no stage runs.
fn client() -> Client {
    let provider: Arc<dyn ProviderAdapter> = Arc::new(ScriptedProvider::new(Vec::new()));
    let build = Client::builder()
        .catalog(catalog())
        .enabled_providers(["test".to_owned()])
        .adapter_arc("test", provider)
        .build()
        .expect("the client builds");
    assert!(build.issues.is_empty(), "{:?}", build.issues);
    build.client
}

/// The runtime a Fabro host builds, with the model catalog installed so
/// the admission pass pins every stage's model.
fn runtime(dir: &RunDir) -> Runtime {
    register(Runtime::standard().frontend(Fabro::new()))
        .capability(PebbleClient(client()))
        .options(RunOptions::new(dir.path()))
}

/// The bundle as a host holds it: the workflow, the settings beside it and
/// the project settings at the root.
fn bundle(workflow: &str) -> MapFiles {
    MapFiles(BTreeMap::from([
        (WORKFLOW_FILE.to_owned(), workflow.to_owned()),
        ("flows/workflow.toml".to_owned(), WORKFLOW_TOML.to_owned()),
        (".fabro/project.toml".to_owned(), PROJECT_TOML.to_owned()),
    ]))
}

/// The inputs both entry points get: the repository variable bound to one
/// value, as a host whose runs check a repository out binds it.
fn inputs() -> CompileInputs {
    CompileInputs::new().with_var(REPOSITORY_VAR, "/srv/repo")
}

fn check_in_memory(rt: &Runtime, workflow: &str) -> Lowered {
    rt.check_source(WORKFLOW_FILE, workflow, &bundle(workflow), None, &inputs())
        .expect("a frontend claims the workflow")
}

fn admitted(lowered: Lowered) -> Graph {
    lowered
        .graph
        .unwrap_or_else(|| panic!("admitted: {:?}", lowered.diagnostics))
}

fn node<'a>(graph: &'a Graph, name: &str) -> &'a Node {
    graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == name)
        .expect("the node")
}

#[test]
fn in_memory_check_matches_the_check_over_the_same_files_on_disk() {
    let dir = RunDir::new("check-source");
    let rt = runtime(&dir);

    let from_memory = check_in_memory(&rt, WORKFLOW);

    let repo = dir.path().join("repo");
    for (path, text) in &bundle(WORKFLOW).0 {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().expect("a parent")).expect("the directory");
        fs::write(file, text).expect("the file");
    }
    let from_disk = rt
        .check(&repo.join(WORKFLOW_FILE), None, Some(&repo), &inputs())
        .expect("a frontend claims the workflow");

    assert_eq!(
        from_memory.diagnostics.iter().collect::<Vec<_>>(),
        from_disk.diagnostics.iter().collect::<Vec<_>>()
    );
    assert_eq!(from_memory.children, from_disk.children);
    let memory_graph = admitted(from_memory);
    assert_eq!(memory_graph, admitted(from_disk));
    assert_eq!(
        memory_graph.params["fabro.launch"]["clone"]["repository"],
        json!("/srv/repo"),
        "the repository variable is the host's, used as given"
    );
}

#[test]
fn in_memory_check_reads_the_settings_files_from_the_map_and_admits() {
    let dir = RunDir::new("check-source-admit");
    let graph = admitted(check_in_memory(&runtime(&dir), WORKFLOW));

    let config = &node(&graph, "a").step.config;
    assert_eq!(
        config["model"],
        json!("model"),
        "`sol` from `.fabro/project.toml` at the root resolved through the catalog"
    );
    assert_eq!(config["provider"], json!("test"));
    assert_eq!(config[PLAN_KEY]["original"]["provider"], json!("test"));
    assert_eq!(config[PLAN_KEY]["original"]["model"], json!("model"));
    assert_eq!(
        config[PLAN_KEY]["remaining"],
        json!([{ "provider": "test", "model": "big" }]),
        "the chain from `flows/workflow.toml` beside the workflow is frozen in the plan"
    );
}

#[test]
fn diagnostics_name_the_in_memory_path() {
    let dir = RunDir::new("check-source-span");
    let rt = runtime(&dir);

    // A frontend diagnostic: an attribute the format does not know.
    let unknown_attribute = WORKFLOW.replace("prompt=\"First.\"", "prompt=\"First.\", bogus=\"1\"");
    let lowered = check_in_memory(&rt, &unknown_attribute);
    assert!(lowered.graph.is_none(), "{:?}", lowered.diagnostics);
    let error = lowered.diagnostics.errors().next().expect("an error");
    assert_eq!(error.span.file, WORKFLOW_FILE, "{error:?}");
    assert!(error.span.line > 0, "{error:?}");

    // An admission diagnostic: a model the catalog cannot resolve.
    let unknown_model = WORKFLOW.replace("prompt=\"First.\"", "prompt=\"First.\", model=\"nope\"");
    let lowered = check_in_memory(&rt, &unknown_model);
    assert!(lowered.graph.is_none(), "{:?}", lowered.diagnostics);
    let error = lowered
        .diagnostics
        .errors()
        .find(|d| d.code == UNKNOWN_CODE)
        .unwrap_or_else(|| panic!("the admission error: {:?}", lowered.diagnostics));
    assert_eq!(error.span.file, WORKFLOW_FILE, "{error:?}");
    assert!(error.span.line > 0, "{error:?}");
}
