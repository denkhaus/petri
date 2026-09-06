//! The Fabro black box battery: the shipped `petri` binary runs complete
//! Fabro workflows against provider twins on loopback, with real shell and
//! file tools in the sandbox workspace, a scripted interviewer for human
//! gates and agent questions, and an isolated environment that cannot reach
//! a live provider.
//!
//! Task 3's parallel regression lives in this file too, under its own test
//! functions and `support/fabro` modules. That regression comes from the
//! readiness assessment: two command branches write distinct findings under the
//! same context key, a fan-in joins them, and the pinned Conveyor
//! `code_review.py` merges them. Petri `9cea20d` emits `{id, status, output}`
//! per branch, so the helper sees no `context_updates` and reports zero
//! findings.
//!
//! Two tests pin today's loss (`current_*`); they pass now and must be deleted
//! by task 6. Two tests state the contract in
//! `crates/fabro/acceptance/scenarios/parallel-results/CONTRACT.md`
//! (`contract_*`); they are `#[should_panic]` on the exact contract marker, so
//! they turn red the moment the fix lands and task 6 removes the attribute.
//! An infrastructure failure panics with a different message and fails either
//! way.

mod support;

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use support::fabro::interview;
use support::fabro::launch::{Case, Launch, sanitized_path};
use support::fabro::twins::{
    Provider, Twin, model, question_tool, requested_effort, scenario, shell_tool, text, tool_call,
};

/// The edit-and-verify workflow: a command prepares a file, a native agent
/// edits it through real tools, a command verifies the edit, and a human
/// gate decides what happens next.
fn edit_and_verify(provider: Provider) -> String {
    format!(
        r#"digraph EditAndVerify {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'draft\n' > notes.txt && echo prepared"]
    agent [prompt="Append the word reviewed to notes.txt, then read it back and say APPENDED.", model="{model}", provider="{provider}", reasoning_effort="high", on_failure="exit"]
    verify [shape=parallelogram, script="cat notes.txt"]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    ship [shape=parallelogram, script="printf 'shipped\n' > decision.txt && cat decision.txt"]
    hold [shape=parallelogram, script="printf 'held\n' > decision.txt && cat decision.txt"]
    start -> prepare -> agent -> verify -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}}"#,
        model = model(provider),
        provider = provider.id(),
    )
}

/// The twin's side of [`edit_and_verify`]: the model appends through the
/// shell tool, reads the file back, and answers.
fn edit_and_verify_scripts(provider: Provider, namespace: &str) -> Vec<Value> {
    edit_and_verify_scripts_prefixed(provider, namespace, "")
}

/// [`edit_and_verify_scripts`] with scenario ids prefixed, so two cases can
/// share one twin fixture file (ids are unique per file).
fn edit_and_verify_scripts_prefixed(
    provider: Provider,
    namespace: &str,
    prefix: &str,
) -> Vec<Value> {
    let shell = shell_tool(provider);
    let model = model(provider);
    vec![
        scenario(
            provider,
            namespace,
            &format!("{prefix}append"),
            model,
            "Append the word reviewed",
            tool_call(
                "append",
                shell,
                json!({ "command": "printf 'reviewed\\n' >> notes.txt && echo APPEND_DONE" }),
            ),
        ),
        scenario(
            provider,
            namespace,
            &format!("{prefix}read-back"),
            model,
            "APPEND_DONE",
            tool_call(
                "read",
                shell,
                json!({ "command": "cat notes.txt && echo READ_DONE" }),
            ),
        ),
        scenario(
            provider,
            namespace,
            &format!("{prefix}answer"),
            model,
            "READ_DONE",
            text("APPENDED: notes.txt now ends with reviewed."),
        ),
    ]
}

/// Run [`edit_and_verify`] through one twin and check everything the plan's
/// acceptance names: real tools in the workspace, the edited file, the
/// twin's request boundary, the interview, and the final context.
async fn edit_and_verify_case(provider: Provider, label: &str) {
    let mut case = Case::new(label);
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        edit_and_verify_scripts(provider, &case.credential),
    )
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(provider), None);
    let script = interview::write(&case.root, "gate", &[interview::entry(
        "hold-it",
        "gate",
        interview::negative(),
    )]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8 path"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );

    // Files: the agent's edit and the gate's decision, in the workspace the
    // run reported and retained.
    let workspaces = finished.reported_workspaces();
    assert_eq!(workspaces, vec![case.workspace()], "{}", finished.stderr);
    assert_eq!(
        fs::read_to_string(case.workspace().join("notes.txt")).expect("notes.txt"),
        "draft\nreviewed\n"
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("decision.txt")).expect("decision.txt"),
        "held\n"
    );

    // Provider requests: every scripted call consumed, in order, nothing
    // unmatched, the node's model and reasoning on the wire.
    assert_eq!(twin.consumed(), ["append", "read-back", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 3, "{requests:?}");
    for request in &requests {
        assert_eq!(request["model"], model(provider));
        assert_eq!(requested_effort(provider, request), Some("high"));
    }
    let last = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        last.contains("READ_DONE"),
        "the tool result returned to the model: {last}"
    );
    assert!(last.contains("reviewed"), "{last}");

    // Interviews: the one gate, answered negatively through the real step.
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]));
    assert_eq!(receipt["questions"].as_array().map(Vec::len), Some(1));
    assert_eq!(receipt["questions"][0]["node"], "gate");
    assert_eq!(receipt["questions"][0]["reply"]["choice"], "N");
    assert_eq!(receipt["questions"][0]["delivery"], "delivered");
    assert_eq!(receipt["script"]["entries"][0]["consumed"], 1);

    // Final context: the agent's response, the last stage, the gate's choice.
    let context = finished.final_context();
    assert_eq!(
        context["response.agent"],
        json!("APPENDED: notes.txt now ends with reviewed.")
    );
    // `last_stage` is the agent handler's key, as in Fabro; commands and gates
    // leave it alone.
    assert_eq!(context["last_stage"], json!("agent"));
    assert_eq!(context["human.gate.selected"], json!("N"));
    assert_eq!(context["command.output"], json!("held\n"));
    assert!(!context.contains_key("response.verify"), "{context:?}");

    // Process and output: every stage finished, attributable output on stderr.
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    for node in ["prepare", "agent", "verify", "gate", "hold"] {
        assert!(nodes.contains(&node.to_owned()), "{node} in {nodes:?}");
    }
    assert!(!nodes.contains(&"ship".to_owned()), "{nodes:?}");
    let echoed = finished.echoed();
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "verify" && line == "reviewed"),
        "{echoed:?}"
    );
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "prepare" && line == "prepared"),
        "{echoed:?}"
    );
    assert!(
        !finished.stderr.contains(&case.credential),
        "the fake credential never reaches the terminal"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

#[tokio::test]
async fn edit_and_verify_through_native_openai() {
    edit_and_verify_case(Provider::OpenAi, "openai").await;
}

#[tokio::test]
async fn edit_and_verify_through_native_anthropic() {
    edit_and_verify_case(Provider::Anthropic, "anthropic").await;
}

/// Two cases against one twin, at the same time, with their own credential
/// namespaces: neither consumes the other's scripts or interview answers.
#[tokio::test]
async fn concurrent_cases_keep_their_scripts_apart() {
    let mut first = Case::new("concurrent-a");
    let mut second = Case::new("concurrent-b");
    let mut scripts = edit_and_verify_scripts_prefixed(Provider::OpenAi, &first.credential, "a/");
    scripts.extend(edit_and_verify_scripts_prefixed(
        Provider::OpenAi,
        &second.credential,
        "b/",
    ));
    let twin = Twin::start(Provider::OpenAi, &first.root.join("twins"), scripts).await;
    first.redirect(&twin);
    second.redirect(&twin);
    let workflow_a = first.workflow(&edit_and_verify(Provider::OpenAi), None);
    let workflow_b = second.workflow(&edit_and_verify(Provider::OpenAi), None);
    let script_a = interview::write(&first.root, "gate", &[interview::entry(
        "a-holds",
        "gate",
        interview::negative(),
    )]);
    let script_b = interview::write(&second.root, "gate", &[interview::entry(
        "b-ships",
        "gate",
        interview::choice("Y"),
    )]);
    let args_a = ["--interview-script", script_a.to_str().expect("utf-8")];
    let args_b = ["--interview-script", script_b.to_str().expect("utf-8")];
    let (a, b) = tokio::join!(
        first.run(&workflow_a, &args_a),
        second.run(&workflow_b, &args_b),
    );
    a.assert_code(0);
    b.assert_code(0);
    assert_eq!(twin.unmatched(), 0);
    let consumed = twin.consumed();
    assert_eq!(consumed.len(), 6, "{consumed:?}");
    for id in [
        "a/append",
        "a/read-back",
        "a/answer",
        "b/append",
        "b/read-back",
        "b/answer",
    ] {
        assert!(consumed.contains(&id.to_owned()), "{id} in {consumed:?}");
    }
    assert_eq!(twin.requests_for(&first.credential).len(), 3);
    assert_eq!(twin.requests_for(&second.credential).len(), 3);
    assert_eq!(
        fs::read_to_string(first.workspace().join("decision.txt")).expect("a"),
        "held\n"
    );
    assert_eq!(
        fs::read_to_string(second.workspace().join("decision.txt")).expect("b"),
        "shipped\n"
    );
    assert_eq!(a.receipt()["script"]["entries"][0]["id"], "a-holds");
    assert_eq!(b.receipt()["script"]["entries"][0]["id"], "b-ships");
    a.assert_no_leaked_processes().await;
    b.assert_no_leaked_processes().await;
}

/// An agent question and a workflow human gate answered by one scripted
/// interviewer: the agent's question rides Pebble's question tool into the
/// same receipt, with its tool call and session in the question identity.
#[tokio::test]
async fn an_agent_question_and_a_human_gate_share_the_scripted_interviewer() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("agent-question");
    let model = model(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "ask",
            model,
            "Ask me which file",
            tool_call(
                "ask",
                question_tool(provider),
                json!({ "questions": [{
                    "id": "which",
                    "header": "File",
                    "question": "Which file should carry the note?",
                    "options": [{ "label": "README" }, { "label": "CHANGELOG" }]
                }] }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "write",
            model,
            "option_2",
            tool_call(
                "write",
                shell_tool(provider),
                json!({ "command": "printf 'CHANGELOG\\n' > chosen.txt && echo WROTE_CHOICE" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "done",
            model,
            "WROTE_CHOICE",
            text("Recorded the choice."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Ask {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Ask me which file should carry the note, then write my answer to chosen.txt.", model="{model}", provider="openai"]
    gate [shape=hexagon, label="Keep going?", question_type="confirmation"]
    finish [shape=parallelogram, script="cat chosen.txt"]
    start -> agent -> gate
    gate -> finish [label="[Y] Yes"]
    gate -> exit [label="[N] No"]
    finish -> exit
}}"#
        ),
        None,
    );
    let script = interview::write(&case.root, "both", &[
        interview::entry_matching(
            "pick-changelog",
            json!({
                "node": "agent",
                "kind": "multiple_choice",
                "text_contains": "Which file should carry the note?",
                "options": ["option_1", "option_2"],
                "freeform": true,
            }),
            1,
            interview::choice("option_2"),
        ),
        interview::entry_matching(
            "keep-going",
            json!({ "node": "gate", "kind": "confirmation", "options": ["Y", "N"] }),
            1,
            interview::choice("Y"),
        ),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(twin.consumed(), ["ask", "write", "done"]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("chosen.txt")).expect("chosen.txt"),
        "CHANGELOG\n"
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]));
    let questions = receipt["questions"].as_array().expect("questions");
    assert_eq!(questions.len(), 2, "{receipt}");
    let agent = questions
        .iter()
        .find(|q| q["node"] == "agent")
        .expect("the agent's question");
    let id = agent["question"].as_str().expect("id");
    assert!(id.contains("/agent/"), "{id}");
    assert!(id.contains("/ask/0"), "tool call id and index: {id}");
    assert_eq!(agent["reply"]["choice"], "option_2");
    assert_eq!(agent["kind"], "multiple_choice");
    let gate = questions
        .iter()
        .find(|q| q["node"] == "gate")
        .expect("the gate's question");
    assert_eq!(gate["reply"]["choice"], "Y");
    assert_eq!(
        finished.final_context()["command.output"],
        json!("CHANGELOG\n")
    );
    // The answer reached the model as the tool's result.
    let requests = twin.requests_for(&case.credential);
    let after_answer = serde_json::to_string(&requests[1]).expect("request");
    assert!(after_answer.contains("option_2"), "{after_answer}");
    finished.assert_no_leaked_processes().await;
}

const GATE_ONLY: &str = r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    ship [shape=parallelogram, script="echo shipped"]
    hold [shape=parallelogram, script="echo held"]
    start -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}"#;

#[tokio::test]
async fn an_unexpected_question_fails_the_interview_and_the_run() {
    let case = Case::new("unexpected-question");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "wrong-node", &[interview::entry(
        "other",
        "not-the-gate",
        interview::choice("Y"),
    )]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("interview verification failed"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    let errors = receipt["errors"].as_array().expect("errors");
    assert!(
        errors.iter().any(|e| e
            .as_str()
            .is_some_and(|e| e.contains("no script entry matches"))),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().is_some_and(|e| e.contains("unused required"))),
        "{errors:?}"
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(!nodes.contains(&"ship".to_owned()) && !nodes.contains(&"hold".to_owned()));
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn an_unused_required_entry_fails_verification_without_rewriting_the_run_status() {
    let case = Case::new("unused-entry");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "extra", &[
        interview::entry("ship-it", "gate", interview::choice("Y")),
        interview::entry("never-asked", "review", interview::choice("Y")),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["questions"][0]["reply"]["choice"], "Y");
    let entries = receipt["script"]["entries"].as_array();
    assert!(
        entries.is_none(),
        "an unused entry is an error, not a summary"
    );
    assert!(
        receipt["errors"][0]
            .as_str()
            .is_some_and(|e| e.contains("`never-asked` answered 0 of 1")),
        "{receipt}"
    );
    // The persisted run stands as the engine reported it.
    let coordinator = fs::read_to_string(case.run_dir.join("coordinator.jsonl")).expect("log");
    assert!(
        coordinator.contains(r#""RunFinished":{"status":"Success"}"#),
        "{coordinator}"
    );
}

#[tokio::test]
async fn the_wrong_model_reaches_no_script_and_fails_the_agent() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("wrong-model");
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        edit_and_verify_scripts(provider, &case.credential),
    )
    .await;
    case.redirect(&twin);
    // The scripts expect gpt-5.6-sol; the workflow asks for terra.
    let dot = edit_and_verify(provider).replace("gpt-5.6-sol", "gpt-5.6-terra");
    let workflow = case.workflow(&dot, None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(twin.unmatched() >= 1, "{:?}", twin.request_log());
    assert!(twin.consumed().is_empty(), "{:?}", twin.consumed());
    assert!(
        twin.requests_for(&case.credential)
            .iter()
            .all(|r| r["model"] == "gpt-5.6-terra")
    );
    assert!(!case.workspace().join("decision.txt").exists());
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn a_scripted_call_that_never_arrives_is_visible_as_unconsumed() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("missing-call");
    let mut scripts = edit_and_verify_scripts(provider, &case.credential);
    scripts.push(scenario(
        provider,
        &case.credential,
        "never-requested",
        model(provider),
        "NO_SUCH_MARKER",
        text("unreachable"),
    ));
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(provider), None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(0);
    let consumed = twin.consumed();
    assert_eq!(consumed, ["append", "read-back", "answer"]);
    assert!(
        !consumed.contains(&"never-requested".to_owned()),
        "the harness sees the missing call: {consumed:?}"
    );
}

#[tokio::test]
async fn malformed_agent_output_fails_the_stage_routably() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("malformed-output");
    let twin = Twin::start(provider, &case.root.join("twins"), vec![scenario(
        provider,
        &case.credential,
        "not-json",
        model(provider),
        "Reply with routing JSON",
        text("I would rather not."),
    )])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Malformed {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Reply with routing JSON.", model="{}", provider="openai", output_schema="routing", output_retries=0, on_failure="exit"]
    start -> agent -> exit
}}"#,
            model(provider)
        ),
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["not-json"]);
    let context = finished.final_context();
    assert_eq!(context["failure_class"], json!("bad_output"), "{context:?}");
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn a_provider_without_a_redirect_is_unreachable() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("no-redirect");
    // Only OpenAI is redirected and enabled; the workflow names Anthropic.
    let twin = Twin::start(provider, &case.root.join("twins"), Vec::new()).await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(Provider::Anthropic), None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(twin.requests().is_empty(), "{:?}", twin.requests());
    assert!(
        !case.workspace().join("notes.txt").exists() || {
            // `prepare` ran before the agent failed; the agent's edit did not.
            fs::read_to_string(case.workspace().join("notes.txt")).expect("notes") == "draft\n"
        }
    );
    finished.assert_no_leaked_processes().await;
}

/// A catalog layer that points at a closed port: the client is built, the
/// request fails at connect, and nothing leaves the loopback.
#[tokio::test]
async fn a_redirect_to_a_closed_port_fails_locally() {
    let mut case = Case::new("closed-port");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    case.redirect_to_nothing(Provider::OpenAi, &format!("http://127.0.0.1:{port}"));
    let workflow = case.workflow(&edit_and_verify(Provider::OpenAi), None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
}

const WORKSPACE_PROBE: &str = r#"digraph Probe {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    write [shape=parallelogram, script="printf 'kept\n' > kept.txt && cat kept.txt"]
    start -> write -> exit
}"#;

#[tokio::test]
async fn fabro_runs_keep_the_workspace_after_success_by_default() {
    let case = Case::new("retain-default");
    let workflow = case.workflow(WORKSPACE_PROBE, None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("kept.txt")).expect("kept.txt"),
        "kept\n"
    );
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn retain_never_deletes_the_workspace() {
    let case = Case::new("retain-never");
    let workflow = case.workflow(WORKSPACE_PROBE, None);
    let finished = case.run(&workflow, &["--retain", "never"]).await;
    finished.assert_code(0);
    assert!(
        !case.workspace().exists(),
        "{} still exists",
        case.workspace().display()
    );
    assert!(
        finished.reported_workspaces().is_empty(),
        "{}",
        finished.stderr
    );
}

#[tokio::test]
async fn a_failed_run_keeps_its_workspace() {
    let case = Case::new("retain-failure");
    let workflow = case.workflow(
        r#"digraph Fail {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    write [shape=parallelogram, script="printf 'partial\n' > partial.txt && exit 3", on_failure="exit"]
    start -> write -> exit
}"#,
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("partial.txt")).expect("partial.txt"),
        "partial\n"
    );
}

#[tokio::test]
async fn a_cancelled_run_keeps_its_workspace_and_stops_its_work() {
    let case = Case::new("retain-cancel");
    let workflow = case.workflow(
        r#"digraph Cancel {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="printf 'started\n' > started.txt; sleep 60; printf 'finished\n' > finished.txt"]
    start -> work -> exit
}"#,
        None,
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            interrupt_when: Some(case.workspace().join("started.txt")),
            ..Launch::default()
        })
        .await;
    assert!(!finished.timed_out, "the interrupt ended the run");
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert!(case.workspace().join("started.txt").exists());
    assert!(!case.workspace().join("finished.txt").exists());
    finished.assert_no_leaked_processes().await;
}

/// `--interactive` with piped input: the prompt shows the question in the
/// gate's presentation and one typed line decides the route.
#[tokio::test]
async fn interactive_input_answers_a_gate_from_the_terminal() {
    let case = Case::new("interactive");
    let workflow = case.workflow(GATE_ONLY, None);
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some("no\n".into()),
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert!(
        finished.stderr.contains("question [gate]: Ship it?"),
        "{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("[Y] Yes  [N] No"),
        "{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("(Enter takes [Y])"),
        "{}",
        finished.stderr
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"hold".to_owned()), "{nodes:?}");
    assert_eq!(finished.receipt()["questions"][0]["reply"]["choice"], "N");
}

#[tokio::test]
async fn interactive_eof_fails_the_gate_closed() {
    let case = Case::new("interactive-eof");
    let workflow = case.workflow(GATE_ONLY, None);
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some(String::new()),
            close_stdin: true,
            ..Launch::default()
        })
        .await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["questions"][0]["reply"]["kind"], "failed");
    assert!(
        receipt["errors"][0]
            .as_str()
            .is_some_and(|e| e.contains("EOF") || e.contains("closed")),
        "{receipt}"
    );
}

/// The flags exclude one another.
#[tokio::test]
async fn interview_options_are_mutually_exclusive() {
    let case = Case::new("exclusive");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "x", &[]);
    let finished = case
        .run(&workflow, &[
            "--auto-approve",
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    assert_eq!(finished.code, Some(2), "{}", finished.stderr);
    assert!(
        finished.stderr.contains("cannot be used with"),
        "{}",
        finished.stderr
    );
}

/// `workflow.toml` beside the workflow: `[run.inputs]` binds an input, and a
/// section the standalone runner does not act on is reported, not silently
/// dropped.
#[tokio::test]
async fn workflow_toml_inputs_bind_and_unsupported_sections_are_reported() {
    let case = Case::new("workflow-toml");
    let workflow = case.workflow(
        r#"digraph Inputs {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    say [shape=parallelogram, script="echo {{ inputs.word }}"]
    start -> say -> exit
}"#,
        Some(
            "[run.inputs]\nword = \"bound\"\n\n[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"claude-sonnet-5\"]\n\n[run.environment]\nid = \"review\"\n",
        ),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(finished.final_context()["command.output"], json!("bound\n"));
    assert!(
        finished
            .stderr
            .contains("ignored.workflow_toml.run.model.fallbacks"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .stderr
            .contains("ignored.workflow_toml.run.environment"),
        "{}",
        finished.stderr
    );
}

/// Readiness milestone A, the terminal smoke run as a test: the shipped
/// binary, with no `fabro` reachable on `PATH` and nothing else from the
/// developer's environment, performs a command, drives a scripted native
/// agent through real tools, accepts a scripted human answer, and leaves the
/// workspace files where the run said they are. Everything the run recorded
/// is then read back through `petri inspect`, the public inspection surface.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr"
)]
async fn milestone_a_smoke_run_without_fabro_on_path() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("milestone-a");
    let Some(path) = sanitized_path(&case.root) else {
        eprintln!("skipping: a fabro executable lives in a system bin directory");
        return;
    };
    // `fabro` really is unreachable under this PATH.
    let probe = std::process::Command::new("fabro")
        .env_clear()
        .env("PATH", &path)
        .arg("--version")
        .output();
    assert!(probe.is_err(), "fabro resolved under the sanitized PATH");

    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        edit_and_verify_scripts(provider, &case.credential),
    )
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(provider), None);
    let script = interview::write(&case.root, "gate", &[interview::entry(
        "hold-it",
        "gate",
        interview::negative(),
    )]);
    let finished = case
        .run_with(
            &workflow,
            &["--interview-script", script.to_str().expect("utf-8 path")],
            Launch {
                path: Some(path),
                ..Launch::default()
            },
        )
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );

    // A command ran, an agent edited through real tools, a human answered,
    // and the files are where the run said.
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("notes.txt")).expect("notes.txt"),
        "draft\nreviewed\n"
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("decision.txt")).expect("decision.txt"),
        "held\n"
    );
    assert_eq!(twin.consumed(), ["append", "read-back", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let echoed = finished.echoed();
    for (node, line) in [
        ("prepare", "prepared"),
        ("verify", "reviewed"),
        ("hold", "held"),
    ] {
        assert!(
            echoed.iter().any(|(n, l)| n == node && l == line),
            "{node}: {echoed:?}"
        );
    }

    // The public inspection surface carries the run, its context, and the
    // interview receipt.
    let document = finished.inspect();
    assert_eq!(
        document["complete"],
        json!(true),
        "{}",
        document["incomplete"]
    );
    assert_eq!(document["status"], json!("success"));
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("N"));
    assert_eq!(context["command.output"], json!("held\n"));
    assert_eq!(
        context["response.agent"],
        json!("APPENDED: notes.txt now ends with reviewed.")
    );
    let receipt = &document["interviews"];
    assert_eq!(receipt["version"], json!(1), "{receipt}");
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(receipt["questions"][0]["node"], json!("gate"));
    assert_eq!(receipt["questions"][0]["reply"]["choice"], json!("N"));
    assert_eq!(receipt["questions"][0]["delivery"], json!("delivered"));
    assert_eq!(receipt["script"]["entries"][0]["id"], json!("hold-it"));
    assert_eq!(*receipt, finished.receipt(), "the document is the file");
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Keep the workspace path formula in one place the tests can see.
#[allow(dead_code, reason = "documents the layout the cases assert against")]
fn workspace_of(run_dir: &Path) -> PathBuf {
    run_dir
        .join("scopes")
        .join("invocation-0-scope-0")
        .join("work")
}

#[allow(dead_code, reason = "a bound every case shares")]
const _: Duration = support::fabro::launch::RUN_DEADLINE;

// ── Task 3: the parallel-result regression ──────────────────────────────────

use support::fabro::{BranchEnvelope, Petri, RunObservation, RunOutput, Scenario};

const SCENARIO: &str = "parallel-results";
const FINDERS: [&str; 2] = ["finder_a", "finder_b"];
const CONTRACT: &str = "parallel result contract (task 6):";

const FINDING_A: &str = "page_count drops the final partial page";
const FINDING_B: &str = "render escapes the title twice";

/// The report the pinned helper writes when both findings survive, as Fabro
/// produced it (`fabro-reference/raw/report.md`).
fn expected_report() -> String {
    fs::read_to_string(Scenario::source_file(
        SCENARIO,
        "fabro-reference/raw/report.md",
    ))
    .expect("the Fabro reference report is tracked")
}

/// The branch envelopes Fabro produced for the finder fan-out, from the
/// normalized capture, without `command.output` (Petri's command output ends
/// with a newline; the value is compared on its own).
fn fabro_finder_envelopes() -> Vec<BranchEnvelope> {
    let text = fs::read_to_string(Scenario::source_file(
        SCENARIO,
        "fabro-reference/normalized.json",
    ))
    .expect("the normalized Fabro capture is tracked");
    let capture: Value = serde_json::from_str(&text).expect("normalized.json is JSON");
    let group = capture["parallel_groups"]
        .as_array()
        .and_then(|groups| groups.iter().find(|g| g["node"] == "find"))
        .expect("the capture has the `find` group");
    group["results_from_dump"]
        .as_array()
        .expect("the group has dumped results")
        .iter()
        .map(|value| {
            let mut value = value.clone();
            if let Some(updates) = value["context_updates"].as_object_mut() {
                updates.remove("command.output");
            }
            BranchEnvelope::from_value(&value)
        })
        .collect()
}

fn run_scenario(label: &str) -> (Scenario, RunOutput) {
    let scenario = Scenario::stage(SCENARIO);
    let helper = scenario.file("helper/code_review.py");
    let output = Petri::run_workflow(&scenario.file("workflow.fabro"), &scenario.run_dir(label))
        .input("helper", helper.to_string_lossy())
        .input("level", "high")
        .input("target", "review-fixture")
        .run();
    assert!(
        output.success(),
        "the scenario runs to completion today:\n{}",
        output.stderr()
    );
    assert_eq!(output.run_status().as_deref(), Some("success"));
    (scenario, output)
}

fn finder_envelopes(observation: &RunObservation) -> Vec<BranchEnvelope> {
    let envelopes = observation
        .fan_in_output("find_join")
        .expect("the finder fan-in produced a list of envelopes");
    let ids: Vec<&str> = envelopes.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, FINDERS, "one envelope per branch, in branch order");
    envelopes
}

/// Strip `command.output` so an envelope compares on the finding it carries.
fn without_command_output(mut envelope: BranchEnvelope) -> BranchEnvelope {
    if let Some(updates) = envelope
        .context_updates
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        updates.remove("command.output");
    }
    envelope
}

#[test]
fn current_branch_envelopes_keep_both_branches_but_drop_their_context() {
    let (_scenario, output) = run_scenario("current-envelopes");
    let observation = RunObservation::load(&output.run_dir);
    let envelopes = finder_envelopes(&observation);

    // Both branches are preserved, in edge order, and each branch's own
    // output still holds its finding.
    assert_eq!(envelopes.len(), 2);
    assert!(
        envelopes[0]
            .output
            .as_ref()
            .is_some_and(|o| o["stdout"].as_str().is_some_and(|s| s.contains(FINDING_A))),
        "{:?}",
        envelopes[0]
    );
    assert!(
        envelopes[1]
            .output
            .as_ref()
            .is_some_and(|o| o["stdout"].as_str().is_some_and(|s| s.contains(FINDING_B))),
        "{:?}",
        envelopes[1]
    );
    // Today's shape: Petri's status tag, no index, no context_updates. Task 6
    // deletes this test when it makes the contract tests pass.
    for envelope in &envelopes {
        assert_eq!(envelope.status.as_deref(), Some("success"));
        assert_eq!(envelope.index, None);
        assert_eq!(envelope.context_updates, None);
    }
}

#[test]
fn current_helper_sees_no_findings() {
    let (_scenario, output) = run_scenario("current-helper");
    let observation = RunObservation::load(&output.run_dir);

    assert!(
        output
            .echoed("merge_find")
            .contains("Pooled 0 candidates into 0 locations"),
        "{}",
        output.stderr()
    );
    let context = observation.final_context();
    assert_eq!(context["candidate_count"], json!(0));
    assert_eq!(context["run_verify"], json!(false));
    assert_eq!(context["reported"], json!(0));
    let report = output.echoed("report");
    assert!(report.contains("## Findings (0)"), "{report}");
    assert!(
        !report.contains(FINDING_A) && !report.contains(FINDING_B),
        "{report}"
    );
}

#[test]
#[should_panic(expected = "parallel result contract (task 6):")]
fn contract_branch_envelopes_carry_index_status_and_context_updates() {
    let (_scenario, output) = run_scenario("contract-envelopes");
    let observation = RunObservation::load(&output.run_dir);
    let envelopes = finder_envelopes(&observation);
    assert_eq!(envelopes.len(), 2, "{CONTRACT} two branches, two envelopes");

    let expected = fabro_finder_envelopes();
    for (position, (actual, fabro)) in envelopes.iter().zip(&expected).enumerate() {
        assert_eq!(actual.id, fabro.id, "{CONTRACT} id at {position}");
        assert_eq!(
            actual.index,
            Some(position as u64),
            "{CONTRACT} index is the edge position at {position}: {actual:?}"
        );
        assert_eq!(
            actual.item_label, None,
            "{CONTRACT} static branches have no item_label: {actual:?}"
        );
        assert_eq!(
            actual.status.as_deref(),
            Some("succeeded"),
            "{CONTRACT} status uses Fabro's vocabulary at {position}: {actual:?}"
        );
        let updates = actual.context_updates.clone().unwrap_or_else(|| {
            panic!("{CONTRACT} context_updates is present at {position}: {actual:?}")
        });
        assert_eq!(
            updates["output.finder"],
            fabro.context_updates.as_ref().expect("fabro has updates")["output.finder"],
            "{CONTRACT} each branch keeps its own output.finder at {position}"
        );
        assert_eq!(
            without_command_output(actual.clone()),
            *fabro,
            "{CONTRACT} envelope {position} matches the Fabro capture"
        );
    }
    // The parent context does not absorb branch-local keys.
    let context = observation.final_context();
    assert!(
        context.get("output.finder").is_none(),
        "{CONTRACT} output.finder stays branch-local: {context}"
    );
}

#[test]
#[should_panic(expected = "parallel result contract (task 6):")]
fn contract_helper_merges_both_findings_into_the_report() {
    let (_scenario, output) = run_scenario("contract-report");
    let observation = RunObservation::load(&output.run_dir);

    let merge_find = output.echoed("merge_find");
    assert!(
        merge_find.contains("Pooled 2 candidates into 2 locations"),
        "{CONTRACT} merge_find pools both candidates:\n{merge_find}"
    );
    let context = observation.final_context();
    assert_eq!(
        context["candidate_count"],
        json!(2),
        "{CONTRACT} candidate_count"
    );
    assert_eq!(context["run_verify"], json!(true), "{CONTRACT} run_verify");
    assert_eq!(
        context["verified_count"],
        json!(2),
        "{CONTRACT} verified_count"
    );
    assert_eq!(context["reported"], json!(2), "{CONTRACT} reported");
    assert_eq!(
        context["parallel.branch_count"],
        json!(2),
        "{CONTRACT} parallel.branch_count is published"
    );
    assert_eq!(
        output.echoed("report"),
        expected_report(),
        "{CONTRACT} the report is byte-identical to Fabro's"
    );
    assert_eq!(observation.final_status(), Some("success"));
    let history: Vec<String> = output
        .node_history()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    for node in [
        "verify",
        "verifier_a",
        "verifier_b",
        "verify_join",
        "merge_verify",
    ] {
        assert!(
            history.iter().any(|n| n == node),
            "{CONTRACT} {node} ran: {history:?}"
        );
    }
}
