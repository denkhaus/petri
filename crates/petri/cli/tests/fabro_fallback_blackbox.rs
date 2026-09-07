//! Readiness item 9a (milestone C1) through the shipped binary: model
//! fallback and failover on `[run.model.fallbacks]`, with the provider twins
//! injecting the failures. Every case runs the real `petri` binary with an
//! isolated environment, one twin per provider on loopback, and reads what
//! the run reported: its status, the workspace, the twins' request logs, and
//! the `fabro.fallback.*` records in the event log the run names.
//!
//! The expected request sequences are derived from the pinned Fabro's
//! source (`handler/llm/api.rs` at `b6482910`: `fallback_plan`,
//! `failover_agent_session`, `complete_one_shot_request`); the reference
//! binary was not run against the twins. Where Petri keeps the conversation
//! across a model change (Fabro rebuilds the session from the original
//! prompt), the case says so.

mod support;

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use support::fabro::failures::{self, any_request, error, hang, refusal, repeated};
use support::fabro::launch::{Case, Launch};
use support::fabro::twins::{
    Provider, Twin, model, requested_effort, scenario, shell_tool, text, tool_call,
};

const OPENAI: Provider = Provider::OpenAi;
const ANTHROPIC: Provider = Provider::Anthropic;
const OPENROUTER: Provider = Provider::OpenRouter;

/// One request each: the client's own retries are off unless a case turns
/// them on, so the twin request logs show exactly the fallback sequence.
fn no_client_retries() -> Launch {
    Launch {
        env: vec![("PETRI_LLM_RETRY_ATTEMPTS".into(), "1".into())],
        ..Launch::default()
    }
}

/// An agent workflow on `primary` with `workflow.toml` chains.
fn agent_workflow(case: &Case, primary: Provider, extra_attrs: &str, toml: &str) -> PathBuf {
    let model = model(primary);
    let provider = primary.id();
    case.workflow(
        &format!(
            r#"digraph Fallback {{
    graph [backend="api", goal="Answer the question"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello to the reviewer.", model="{model}", provider="{provider}", on_failure="exit"{extra_attrs}]
    start -> agent -> exit
}}"#
        ),
        Some(toml),
    )
}

fn chain_openai_to_anthropic() -> &'static str {
    "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"anthropic:claude-sonnet-5\"]\n"
}

/// The requests the twin logged for this case.
fn requests(twin: &Twin, case: &Case) -> Vec<Value> {
    twin.requests_for(&case.credential)
}

fn plan_routes(records: &[Value]) -> Vec<String> {
    let plan = failures::of_node(records, "agent", "fabro.fallback.plan");
    assert_eq!(plan.len(), 1, "one plan per stage: {records:?}");
    plan[0]["routes"]
        .as_array()
        .expect("routes")
        .iter()
        .map(failures::route)
        .collect()
}

/// The primary answers: the plan names the chain and nothing advances.
#[tokio::test]
async fn a_successful_primary_request_never_leaves_its_route() {
    let mut case = Case::new("fallback-primary-ok");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary",
        model(OPENAI),
        "Say hello",
        text("Hello from the primary."),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary"]);
    assert!(
        requests(&anthropic, &case).is_empty(),
        "the fallback never ran"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Hello from the primary.")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    assert_eq!(failures::kinds(&records, "agent"), [
        "plan", "route", "usage"
    ]);
    let route = &failures::of_node(&records, "agent", "fabro.fallback.route")[0];
    assert_eq!(route["position"], json!(0));
    assert_eq!(route["reused"], json!(false));
    assert!(route["session"].is_string(), "{route}");
    let usage = &failures::of_node(&records, "agent", "fabro.fallback.usage")[0];
    assert_eq!(usage["outcome"], json!("ok"));
    assert_eq!(usage["position"], json!(0));
    assert_eq!(failures::route(usage), "openai/gpt-5.6-sol");
    assert!(
        !finished.stderr.contains("model fallback:"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A server error on the primary is eligible: the same prompt goes to the
/// next provider, which answers, and the stage succeeds on that route.
#[tokio::test]
async fn a_qualifying_failure_moves_the_prompt_to_the_next_provider() {
    let mut case = Case::new("fallback-qualifying");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(500, "server_error", "server_error", "the primary is down"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "fallback-answers",
            model(ANTHROPIC),
            "Say hello",
            text("Hello from the fallback."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert_eq!(anthropic.consumed(), ["fallback-answers"]);
    assert_eq!(requests(&openai, &case).len(), 1, "no client retry");
    let fallback = requests(&anthropic, &case);
    assert_eq!(fallback.len(), 1);
    let sent = serde_json::to_string(&fallback[0]).expect("request");
    assert!(
        sent.contains("Say hello to the reviewer."),
        "the fallback gets the prompt itself, not a continuation: {sent}"
    );
    assert!(
        !sent.contains("moved to another model"),
        "no continuation text when no work happened: {sent}"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Hello from the fallback.")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(failures::kinds(&records, "agent"), [
        "plan", "route", "usage", "failover", "route", "usage"
    ]);
    let failover = &failures::of_node(&records, "agent", "fabro.fallback.failover")[0];
    assert_eq!(failover["position"], json!(1));
    assert_eq!(
        failover["from"],
        json!({ "provider": "openai", "model": "gpt-5.6-sol" })
    );
    assert_eq!(failover["to"]["provider"], json!("anthropic"));
    assert_eq!(failover["to"]["model"], json!("claude-sonnet-5"));
    assert_eq!(
        failover["original"],
        json!({ "provider": "openai", "model": "gpt-5.6-sol" })
    );
    assert_eq!(failover["error"]["kind"], json!("server"));
    assert_eq!(failover["error"]["status"], json!(500));
    assert_eq!(failover["error"]["eligible"], json!(true));
    assert_eq!(failover["continuation"], json!("replay_prompt"));
    let usages = failures::of_node(&records, "agent", "fabro.fallback.usage");
    assert_eq!(
        (
            usages[0]["position"].as_u64(),
            usages[0]["outcome"].as_str()
        ),
        (Some(0), Some("error"))
    );
    assert_eq!(
        (
            usages[1]["position"].as_u64(),
            usages[1]["outcome"].as_str()
        ),
        (Some(1), Some("ok"))
    );
    assert_eq!(failures::route(usages[1]), "anthropic/claude-sonnet-5");
    assert!(
        finished
            .stderr
            .contains("model fallback: openai/gpt-5.6-sol failed (server); continuing on anthropic/claude-sonnet-5 (attempt 1 of the plan)"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// An invalid request is deterministic: no fallback starts, the stage fails
/// with the typed class, and the next provider sees nothing.
#[tokio::test]
async fn a_non_qualifying_failure_fails_the_stage_without_fallback() {
    let mut case = Case::new("fallback-ineligible");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "bad-request",
        model(OPENAI),
        "Say hello",
        error(
            400,
            "invalid_request_error",
            "invalid_request",
            "bad request shape",
        ),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["bad-request"]);
    assert!(requests(&anthropic, &case).is_empty());
    let context = finished.final_context();
    assert_eq!(
        context["failure_class"],
        json!("llm:invalid_request"),
        "{context:?}"
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(failures::kinds(&records, "agent"), [
        "plan", "route", "usage", "stop"
    ]);
    let stop = &failures::of_node(&records, "agent", "fabro.fallback.stop")[0];
    assert_eq!(stop["reason"], json!("ineligible"));
    assert_eq!(stop["position"], json!(0));
    assert_eq!(stop["error"]["kind"], json!("invalid_request"));
    assert_eq!(stop["error"]["eligible"], json!(false));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// Three providers: authentication fails on the first, the second is
/// overloaded, the third answers. The failover events chain, each `to` the
/// next `from`.
#[tokio::test]
async fn a_later_provider_succeeds_after_two_failures() {
    let mut case = Case::new("fallback-third");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "no-auth",
        model(OPENAI),
        "Say hello",
        error(401, "authentication_error", "invalid_api_key", "bad key"),
    )])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![
        any_request(
            OPENROUTER,
            &case.credential,
            "overloaded",
            "moonshotai/kimi-k3",
            error(503, "server_error", "service_unavailable", "overloaded"),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "third-answers",
            model(ANTHROPIC),
            "Say hello",
            text("Third time lucky."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&openrouter);
    case.redirect(&anthropic);
    let workflow = agent_workflow(
        &case,
        OPENAI,
        "",
        "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"openrouter:kimi-k3\", \"anthropic:claude-sonnet-5\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["no-auth"]);
    assert_eq!(openrouter.consumed(), ["overloaded"]);
    assert_eq!(anthropic.consumed(), ["third-answers"]);
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Third time lucky.")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "openrouter/kimi-k3",
        "anthropic/claude-sonnet-5"
    ]);
    let failovers = failures::of_node(&records, "agent", "fabro.fallback.failover");
    assert_eq!(failovers.len(), 2);
    assert_eq!(failovers[0]["error"]["kind"], json!("authentication"));
    assert_eq!(failovers[0]["to"]["provider"], json!("openrouter"));
    assert_eq!(
        failovers[1]["from"],
        json!({ "provider": "openrouter", "model": "kimi-k3" })
    );
    assert_eq!(failovers[1]["error"]["kind"], json!("server"));
    assert_eq!(failovers[1]["to"]["provider"], json!("anthropic"));
    assert_eq!(failovers[1]["position"], json!(2));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    openrouter.stop();
    anthropic.stop();
}

/// Every route fails: the stage fails with the last route's typed error,
/// the plan reports exhaustion, and the outcome keeps the position reached.
#[tokio::test]
async fn chain_exhaustion_fails_the_stage_with_the_last_error() {
    let mut case = Case::new("fallback-exhausted");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "no-auth",
        model(OPENAI),
        "Say hello",
        error(401, "authentication_error", "invalid_api_key", "bad key"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "no-quota",
            model(ANTHROPIC),
            "Say hello",
            error(
                429,
                "rate_limit_error",
                "insufficient_quota",
                "credit spent",
            ),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["no-auth"]);
    assert_eq!(anthropic.consumed(), ["no-quota"]);
    let context = finished.final_context();
    assert_eq!(
        context["failure_class"],
        json!("llm:quota_exceeded"),
        "{context:?}"
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(failures::kinds(&records, "agent"), [
        "plan", "route", "usage", "failover", "route", "usage", "stop"
    ]);
    let stop = &failures::of_node(&records, "agent", "fabro.fallback.stop")[0];
    assert_eq!(stop["reason"], json!("exhausted"));
    assert_eq!(stop["position"], json!(1));
    assert_eq!(failures::route(stop), "anthropic/claude-sonnet-5");
    assert_eq!(stop["error"]["kind"], json!("quota_exceeded"));
    assert_eq!(stop["error"]["eligible"], json!(true));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A provider interruption after a non-idempotent tool effect: the primary
/// asks for an append, the append runs, the primary's next request fails,
/// and the fallback continues from the tool result. The append happens once
/// and the next model sees its output. Fabro rebuilds the session from the
/// original prompt here, which would run the tool again; Petri keeps the
/// conversation.
#[tokio::test]
async fn a_tool_effect_is_not_repeated_across_a_failover() {
    let mut case = Case::new("fallback-tool-effect");
    let shell = shell_tool(OPENAI);
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![
        scenario(
            OPENAI,
            &case.credential,
            "append",
            model(OPENAI),
            "Say hello",
            tool_call(
                "append",
                shell,
                json!({ "command": "echo appended >> effects.log && echo APPEND_DONE" }),
            ),
        ),
        scenario(
            OPENAI,
            &case.credential,
            "primary-down",
            model(OPENAI),
            "APPEND_DONE",
            error(503, "server_error", "service_unavailable", "gone away"),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "continues",
            model(ANTHROPIC),
            "moved to another model",
            text("Appended once, as asked."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["append", "primary-down"]);
    assert_eq!(anthropic.consumed(), ["continues"]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("effects.log")).expect("effects.log"),
        "appended\n",
        "the append ran once"
    );
    let fallback = requests(&anthropic, &case);
    assert_eq!(fallback.len(), 1);
    let sent = serde_json::to_string(&fallback[0]).expect("request");
    assert!(
        sent.contains("APPEND_DONE"),
        "the tool result reached the next model: {sent}"
    );
    assert!(
        sent.contains("Say hello to the reviewer."),
        "the original prompt is in the conversation: {sent}"
    );
    assert!(
        sent.contains("do not run those tools again"),
        "the continuation asks the model to go on: {sent}"
    );
    let records = failures::records(&finished.run_dir);
    let failover = &failures::of_node(&records, "agent", "fabro.fallback.failover")[0];
    assert_eq!(failover["continuation"], json!("continue_turn"));
    assert_eq!(failover["error"]["kind"], json!("server"));
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Appended once, as asked.")
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// Cancellation while the fallback route is active: the fallback's tool
/// call marks the workspace, the person interrupts, and the run is
/// cancelled without another decision or request.
#[tokio::test]
async fn cancellation_during_fallback_cancels_the_run() {
    let mut case = Case::new("fallback-cancel");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "slow-work",
            model(ANTHROPIC),
            "Say hello",
            tool_call(
                "slow",
                shell_tool(ANTHROPIC),
                json!({ "command": "touch fallback-active.txt && sleep 30 && echo NEVER" }),
            ),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let launch = Launch {
        interrupt_when: Some(case.workspace().join("fallback-active.txt")),
        ..no_client_retries()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert_eq!(anthropic.consumed(), ["slow-work"]);
    assert_eq!(
        requests(&anthropic, &case).len(),
        1,
        "no request after the interrupt"
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(
        failures::kinds(&records, "agent"),
        ["plan", "route", "usage", "failover", "route", "usage"],
        "the interrupted turn is accounted, no stop decision follows: {records:?}"
    );
    let interrupted = failures::of_node(&records, "agent", "fabro.fallback.usage")[1];
    assert_eq!(interrupted["position"], json!(1));
    assert_eq!(interrupted["outcome"], json!("error"));
    assert_eq!(
        interrupted["usage"]["input"],
        json!(10),
        "the tool-call response the fallback accepted before the interrupt is counted"
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A refusal is the one content filter the reference fails over on; any
/// other content filter is deterministic and ends the stage.
#[tokio::test]
async fn a_refusal_is_eligible_but_another_content_filter_is_not() {
    // Anthropic primary refuses; OpenAI answers.
    let mut case = Case::new("fallback-refusal");
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "refuses",
            model(ANTHROPIC),
            "Say hello",
            refusal(ANTHROPIC),
        ),
    ])
    .await;
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "answers",
        model(OPENAI),
        "Say hello",
        text("Hello, no refusal here."),
    )])
    .await;
    case.redirect(&anthropic);
    case.redirect(&openai);
    let workflow = agent_workflow(
        &case,
        ANTHROPIC,
        "",
        "[run.model.fallbacks]\n\"claude-sonnet-5\" = [\"openai:gpt-5.6-sol\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(anthropic.consumed(), ["refuses"]);
    assert_eq!(openai.consumed(), ["answers"]);
    let records = failures::records(&finished.run_dir);
    let failover = &failures::of_node(&records, "agent", "fabro.fallback.failover")[0];
    assert_eq!(failover["error"]["kind"], json!("content_filter"));
    assert_eq!(failover["error"]["provider_code"], json!("refusal"));
    assert_eq!(failover["error"]["eligible"], json!(true));
    finished.assert_no_leaked_processes().await;
    anthropic.stop();
    openai.stop();

    // OpenAI primary filtered without a refusal code: no fallback.
    let mut case = Case::new("fallback-content-filter");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "filtered",
        model(OPENAI),
        "Say hello",
        error(
            400,
            "invalid_request_error",
            "content_filter",
            "blocked by policy",
        ),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert!(requests(&anthropic, &case).is_empty());
    assert_eq!(
        finished.final_context()["failure_class"],
        json!("llm:content_filter")
    );
    let records = failures::records(&finished.run_dir);
    let stop = &failures::of_node(&records, "agent", "fabro.fallback.stop")[0];
    assert_eq!(stop["reason"], json!("ineligible"));
    assert_eq!(stop["error"]["provider_code"], json!("content_filter"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A request that exceeds the client's call budget is a timeout, which is
/// eligible: the next provider answers.
#[tokio::test]
async fn a_request_timeout_is_eligible() {
    let mut case = Case::new("fallback-timeout");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "hangs",
        model(OPENAI),
        "Say hello",
        hang(),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "answers",
            model(ANTHROPIC),
            "Say hello",
            text("Answered in time."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let launch = Launch {
        env: vec![
            ("PETRI_LLM_RETRY_ATTEMPTS".into(), "1".into()),
            ("PETRI_LLM_TIMEOUT_MS".into(), "1500".into()),
        ],
        ..Launch::default()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    finished.assert_code(0);
    assert_eq!(openai.consumed(), ["hangs"]);
    assert_eq!(anthropic.consumed(), ["answers"]);
    let records = failures::records(&finished.run_dir);
    let failover = &failures::of_node(&records, "agent", "fabro.fallback.failover")[0];
    assert_eq!(failover["error"]["kind"], json!("timeout"), "{failover}");
    assert_eq!(failover["error"]["eligible"], json!(true));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// The client's own retries run before any fallback decision: with two
/// attempts, the primary is asked twice, then the chain advances once.
#[tokio::test]
async fn client_retries_are_spent_before_the_chain_advances() {
    let mut case = Case::new("fallback-client-retries");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![repeated(
        scenario(
            OPENAI,
            &case.credential,
            "flaky",
            model(OPENAI),
            "Say hello",
            error(503, "server_error", "service_unavailable", "flaky"),
        ),
        2,
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "answers",
            model(ANTHROPIC),
            "Say hello",
            text("Steady."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let launch = Launch {
        env: vec![("PETRI_LLM_RETRY_ATTEMPTS".into(), "2".into())],
        ..Launch::default()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    finished.assert_code(0);
    assert_eq!(
        openai.consumed(),
        ["flaky", "flaky"],
        "two sends of one request"
    );
    assert_eq!(requests(&openai, &case).len(), 2);
    assert_eq!(anthropic.consumed(), ["answers"]);
    let records = failures::records(&finished.run_dir);
    assert_eq!(
        failures::of_node(&records, "agent", "fabro.fallback.failover").len(),
        1,
        "one fallback decision, whatever the client retried: {records:?}"
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A workflow retry is a new firing with its own plan at position 0, not a
/// failover: a stage whose failed outcome routes back to itself asks the
/// primary again and never reaches the chain, whatever the chain says.
#[tokio::test]
async fn a_workflow_retry_is_not_a_failover() {
    let mut case = Case::new("fallback-workflow-retry");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![
        scenario(
            OPENAI,
            &case.credential,
            "first-firing",
            model(OPENAI),
            "Say hello",
            text(r#"{"outcome": "failed", "failure_reason": "not yet"}"#),
        ),
        scenario(
            OPENAI,
            &case.credential,
            "second-firing",
            model(OPENAI),
            "Say hello",
            text(r#"{"outcome": "succeeded"}"#),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let model = model(OPENAI);
    let workflow = case.workflow(
        &format!(
            r#"digraph Retry {{
    graph [backend="api", goal="Answer the question"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello to the reviewer.", model="{model}", provider="openai", output_schema="routing", max_visits=3]
    start -> agent
    agent -> agent [condition="outcome=failed"]
    agent -> exit
}}"#
        ),
        Some(chain_openai_to_anthropic()),
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["first-firing", "second-firing"]);
    assert!(
        requests(&anthropic, &case).is_empty(),
        "the chain never ran"
    );
    let records = failures::records(&finished.run_dir);
    let plans = failures::of_node(&records, "agent", "fabro.fallback.plan");
    assert_eq!(plans.len(), 2, "one plan per firing: {records:?}");
    assert_ne!(plans[0]["firing"], plans[1]["firing"]);
    assert!(failures::of_node(&records, "agent", "fabro.fallback.failover").is_empty());
    assert!(failures::of_node(&records, "agent", "fabro.fallback.stop").is_empty());
    let routes = failures::of_node(&records, "agent", "fabro.fallback.route");
    assert_eq!(routes.len(), 2);
    assert!(
        routes.iter().all(|r| r["position"] == json!(0)),
        "{routes:?}"
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// Reasoning effort maps per target through the catalog: a target with no
/// nearby level is skipped with a `NoNearbyReasoningLevel` warning, one
/// that advertises no levels keeps the request, and a chain left with no
/// usable target warns `ChainEmpty` and runs on the primary alone.
#[tokio::test]
async fn reasoning_effort_maps_per_target_and_unfit_targets_are_skipped() {
    let mut case = Case::new("fallback-effort");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![]).await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "answers",
            model(ANTHROPIC),
            "Say hello",
            text("Thought hard."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&openrouter);
    case.redirect(&anthropic);
    // nemotron on OpenRouter advertises no reasoning at all; Claude Sonnet 5
    // advertises reasoning with unspecified levels.
    let workflow = agent_workflow(
        &case,
        OPENAI,
        ", reasoning_effort=\"high\"",
        "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"openrouter:nemotron-3-super-120b-a12b\", \"anthropic:claude-sonnet-5\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert!(
        requests(&openrouter, &case).is_empty(),
        "the unfit target never ran"
    );
    assert_eq!(anthropic.consumed(), ["answers"]);
    let fallback = requests(&anthropic, &case);
    assert_eq!(requested_effort(ANTHROPIC, &fallback[0]), Some("high"));
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    let plan = &failures::of_node(&records, "agent", "fabro.fallback.plan")[0];
    assert_eq!(plan["routes"][1]["reasoning_effort"], json!("high"));
    let notices = plan["notices"].as_array().expect("notices");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["code"], json!("model_fallback_skipped"));
    assert!(
        notices[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("no reasoning level near `high`")),
        "{notices:?}"
    );
    assert!(
        finished.stderr.contains("warn: Model fallback `openrouter:nemotron-3-super-120b-a12b` for requested model `gpt-5.6-sol` was skipped because it has no reasoning level near `high`."),
        "{}",
        finished.stderr
    );
    let failover = &failures::of_node(&records, "agent", "fabro.fallback.failover")[0];
    assert_eq!(failover["requested_reasoning_effort"], json!("high"));
    assert_eq!(failover["to"]["reasoning_effort"], json!("high"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    openrouter.stop();
    anthropic.stop();

    // Only the unfit target: the chain is empty and the failure stands.
    let mut case = Case::new("fallback-chain-empty");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&openrouter);
    let workflow = agent_workflow(
        &case,
        OPENAI,
        ", reasoning_effort=\"high\"",
        "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"openrouter:nemotron-3-super-120b-a12b\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert!(requests(&openrouter, &case).is_empty());
    assert_eq!(
        finished.final_context()["failure_class"],
        json!("llm:server")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), ["openai/gpt-5.6-sol"]);
    let plan = &failures::of_node(&records, "agent", "fabro.fallback.plan")[0];
    let codes: Vec<&str> = plan["notices"]
        .as_array()
        .expect("notices")
        .iter()
        .filter_map(|n| n["code"].as_str())
        .collect();
    assert_eq!(codes, [
        "model_fallback_skipped",
        "model_fallback_chain_empty"
    ]);
    assert!(
        finished
            .stderr
            .contains("warn: No usable model fallbacks remain for requested model `gpt-5.6-sol`"),
        "{}",
        finished.stderr
    );
    let stop = &failures::of_node(&records, "agent", "fabro.fallback.stop")[0];
    assert_eq!(stop["reason"], json!("exhausted"));
    assert_eq!(stop["position"], json!(0));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    openrouter.stop();
}
