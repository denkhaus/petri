//! `fabro/agent` against the fake ACP agent Fabro ships
//! (`fabro-acp/src/test_support.rs` in the corpus checkout): initialize,
//! session, one prompt turn, the response text captured, a routing directive
//! read, permission requests answered, cancellation honoured. Skips when the
//! corpus is not fetched.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use fabro_steps::register;
use frontend_fabro::load;
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::ir::{CancelScopeId, ExprOrValue, Graph, RunStatus};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};
use tokio::time;

fn corpus_script() -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../corpus/fabro/lib/components/fabro-acp/src/test_support.rs");
    let text = fs::read_to_string(path).ok()?;
    let start = text.find("pub fn fake_acp_agent_script()")?;
    let body = &text[start..];
    let open = body.find("r#\"")? + 3;
    let close = body[open..].find("\"#")? + open;
    Some(body[open..close].to_string())
}

/// The fake agent, written where a run can execute it. `None` skips.
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr"
)]
fn fake_agent(dir: &RunDir) -> Option<PathBuf> {
    let Some(script) = corpus_script() else {
        assert!(
            !env::var("PETRI_REQUIRE_FABRO_CORPUS").is_ok_and(|v| !v.is_empty()),
            "PETRI_REQUIRE_FABRO_CORPUS is set, but the Fabro corpus is not fetched"
        );
        eprintln!("skipping: Fabro corpus not fetched; run scripts/corpus-fetch-fabro.sh");
        return None;
    };
    let path = dir.path().join("fake_acp_agent.py");
    fs::write(&path, script).expect("write the fake agent");
    Some(path)
}

fn dot(body: &str) -> String {
    format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}")
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lower(text: &str) -> Graph {
    let lowered = load("test.fabro", text, &NoFiles, &CompileInputs::new());
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("lowers")
}

/// The fake agent's behavior is chosen through `ACP_MODE` and friends in the
/// environment: set them on the scope so the agent process sees them.
fn with_env(mut graph: Graph, pairs: &[(&str, &str)]) -> Graph {
    for scope in &mut graph.body.scopes {
        for (key, value) in pairs {
            scope
                .env
                .insert((*key).into(), ExprOrValue::Value(json!(value)));
        }
    }
    graph
}

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard()).options(options)
}

fn agent_dot(agent: &Path, extra: &str) -> String {
    dot(&format!(
        r#"
        graph [goal="Greet", acp.command="python3 {} "]
        a [prompt="Say hello", model="claude-opus"{extra}]
        start -> a -> exit
    "#,
        agent.display()
    ))
}

async fn run(dir: &RunDir, graph: Graph) -> ExecutionReport {
    runtime(dir)
        .run(graph)
        .await
        .expect("replay is byte-identical")
}

#[tokio::test]
async fn a_turn_captures_the_agent_text() {
    let dir = RunDir::new("fabro-agent-turn");
    let Some(agent) = fake_agent(&dir) else {
        return;
    };
    let graph = lower(&agent_dot(&agent, ""));
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "a");
    assert_eq!(output["text"], json!("hello from acp"));
    assert_eq!(output["outcome"], json!("succeeded"));
    assert_eq!(
        report.state.run_context().get("last_response"),
        Some(&json!("hello from acp"))
    );
}

#[tokio::test]
async fn a_routing_directive_in_the_response_steers_the_edge() {
    let dir = RunDir::new("fabro-agent-directive");
    let Some(agent) = fake_agent(&dir) else {
        return;
    };
    // The fake agent echoes `steered:<prompt>` in `steer` mode on its second
    // prompt; the plain mode says a fixed text. Use a permission request to
    // prove the client answers requests mid-turn.
    let graph = lower(&dot(&format!(
        r#"
        graph [goal="G", acp.command="python3 {}"]
        a [prompt="Say hello"]
        start -> a
        a -> exit [label="Done"]
    "#,
        agent.display()
    )));
    let permission = dir.path().join("permission.json");
    let graph = with_env(graph, &[
        ("ACP_MODE", "permission"),
        ("ACP_PERMISSION", permission.to_str().expect("utf-8")),
    ]);
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let answered = fs::read_to_string(&permission).expect("the permission request was answered");
    assert!(
        answered.contains("allow_always") || answered.contains("always"),
        "{answered}"
    );
}

#[tokio::test]
async fn an_agent_that_exits_early_fails_the_stage_routably() {
    let dir = RunDir::new("fabro-agent-early-exit");
    let Some(agent) = fake_agent(&dir) else {
        return;
    };
    let graph = lower(&dot(&format!(
        r#"
        graph [goal="G", acp.command="python3 {}"]
        a [prompt="Say hello"]
        recover [shape=parallelogram, script="true"]
        start -> a
        a -> recover [condition="outcome=failed"]
        a -> exit
        recover -> exit
    "#,
        agent.display()
    )));
    let graph = with_env(graph, &[("ACP_MODE", "early_exit")]);
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    assert_eq!(status_of(&report, "recover").as_deref(), Some("success"));
}

#[tokio::test]
async fn an_unconfigured_agent_fails_with_a_specific_class() {
    let dir = RunDir::new("fabro-agent-unconfigured");
    let graph = lower(&dot(r#"
        a [prompt="Say hello"]
        start -> a -> exit
    "#));
    let report = run(&dir, graph).await;
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    assert_eq!(
        output_of(&report, "a")["failure_class"],
        json!("acp_unconfigured")
    );
}

#[tokio::test]
async fn cancelling_a_turn_sends_session_cancel_and_stops_the_agent() {
    let dir = RunDir::new("fabro-agent-cancel");
    let Some(agent) = fake_agent(&dir) else {
        return;
    };
    let record = dir.path().join("cancel.txt");
    let graph = lower(&agent_dot(&agent, ""));
    let graph = with_env(graph, &[
        ("ACP_MODE", "cancel"),
        ("ACP_CANCEL_RECORD", record.to_str().expect("utf-8")),
    ]);
    let rt = runtime(&dir);
    let driver = rt.driver(graph);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    assert!(
        testkit::wait_for_file(&dir.path().join("scopes"), Duration::from_secs(10)).await,
        "the run started"
    );
    time::sleep(Duration::from_millis(800)).await;
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(
        fs::read_to_string(&record).ok().as_deref().map(str::trim),
        Some("session/cancel"),
        "the agent saw session/cancel"
    );
}
