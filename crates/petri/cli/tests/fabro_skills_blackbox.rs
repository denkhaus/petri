//! Readiness item 9c (milestone C3) through the shipped binary: Fabro's
//! skill directories and precedence, the reference prompt section and skill
//! tool, a skill driving a real tool under the configured tool hooks, and
//! branch isolation of a loaded skill. Provider twins on loopback, real
//! Pebble, real shell tools, the versioned fixtures in
//! `crates/fabro/acceptance/testdata/skills`, no Fabro, no live provider.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::fabro::launch::{Case, Launch};
use support::fabro::twins::{Provider, Twin, model, scenario, shell_tool, text, tool_call};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fabro/acceptance/testdata/skills")
}

/// A `prepare` node that makes the workspace a Git repository holding the
/// `repo` fixture: the way a Fabro run works at a checkout's root.
fn prepare_repository() -> String {
    format!(
        r#"prepare [shape=parallelogram, script="cp -R '{}/.' . && git init -q && printf 'precious\n' > important.txt && echo prepared"]"#,
        fixtures().join("repo").display()
    )
}

/// The launch that names the fixture `home` as the Fabro home.
fn launch_with_home() -> Launch {
    Launch {
        env: vec![(
            "FABRO_HOME".to_owned(),
            fixtures().join("home").display().to_string(),
        )],
        ..Launch::default()
    }
}

/// The system text a request carries: OpenAI Responses `instructions` and
/// `input` items in the `system` or `developer` role, Anthropic `system` (a
/// string or text blocks).
fn system_text(request: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(text) = request["instructions"].as_str() {
        parts.push(text.to_owned());
    }
    for item in request["input"].as_array().into_iter().flatten() {
        if item["role"] == "system" || item["role"] == "developer" {
            match &item["content"] {
                Value::String(text) => parts.push(text.clone()),
                Value::Array(blocks) => {
                    parts.extend(
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str().map(str::to_owned)),
                    );
                }
                _ => {}
            }
        }
    }
    match &request["system"] {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(blocks) => {
            parts.extend(
                blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str().map(str::to_owned)),
            );
        }
        _ => {}
    }
    parts.join("\n")
}

/// The tool definition named `name` as the request carried it.
fn tool_named<'a>(request: &'a Value, name: &str) -> Option<&'a Value> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["name"] == name)
}

fn expected(file: &str) -> String {
    fs::read_to_string(fixtures().join("expected").join(file)).expect(file)
}

fn expected_json(file: &str) -> Value {
    serde_json::from_str(&expected(file)).expect(file)
}

/// Every object with `"kind": kind` in the run's persisted event logs, root
/// and child invocations alike.
fn persisted_events(run_dir: &Path, kind: &str) -> Vec<Value> {
    fn collect(value: &Value, kind: &str, out: &mut Vec<Value>) {
        match value {
            Value::Object(map) => {
                if map.get("kind").and_then(Value::as_str) == Some(kind) {
                    out.push(value.clone());
                }
                for child in map.values() {
                    collect(child, kind, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect(item, kind, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let executions = run_dir.join("executions");
    for execution in fs::read_dir(&executions).into_iter().flatten().flatten() {
        let log = execution.path().join("events.jsonl");
        let Ok(text) = fs::read_to_string(&log) else {
            continue;
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                collect(&value, kind, &mut out);
            }
        }
    }
    out
}

/// The configured directory, `.fabro/skills` and `skills` are searched in
/// Fabro's order, the repository's `greet` wins over the project and home
/// copies, the prompt section and the `use_skill` tool match the reference,
/// the model's skill call returns the winning template, and the three
/// broken skill files are reported on the terminal and in the event log.
#[tokio::test]
async fn skills_resolve_in_fabros_order_and_the_repository_wins_through_the_binary() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("skills-order");
    let model = model(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "load",
            model,
            "Greet Ada",
            tool_call("load", "use_skill", json!({ "skill_name": "greet" })),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model,
            "REPOSITORY GREETING",
            text("Hello from the repository skill."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Skills {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    {prepare}
    agent [prompt="Greet Ada with the greet skill.", model="{model}", provider="openai", on_failure="exit"]
    start -> prepare -> agent -> exit
}}"#,
            prepare = prepare_repository(),
        ),
        None,
    );
    let finished = case.run_with(&workflow, &[], launch_with_home()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["load", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 2, "{requests:?}");

    let system = system_text(&requests[0]);
    let section = expected("prompt-section.use_skill.txt");
    assert!(
        system.contains(section.trim_end()),
        "the skills section is the reference text:\n{system}"
    );
    let tool = tool_named(&requests[0], "use_skill").expect("the skill tool is offered");
    let reference = expected_json("tool.use_skill.json");
    assert_eq!(tool["description"], reference["description"]);
    assert_eq!(tool["parameters"], reference["parameters"], "{tool}");

    let second = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        second.contains("REPOSITORY GREETING:"),
        "the winning template came back from the tool: {second}"
    );
    assert!(!second.contains("PROJECT GREETING"), "{second}");
    assert!(!second.contains("HOME GREETING"), "{second}");
    let context = finished.final_context();
    assert_eq!(
        context["response.agent"],
        json!("Hello from the repository skill.")
    );

    // Petri's record of the directories, in Fabro's order.
    let resolved = persisted_events(&case.run_dir, "fabro.skills");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    let sources: Vec<&str> = resolved[0]["dirs"]
        .as_array()
        .expect("dirs")
        .iter()
        .map(|d| d["source"].as_str().expect("source"))
        .collect();
    assert_eq!(sources, ["configured", "project_fabro", "project"]);
    assert_eq!(resolved[0]["node"], "agent");
    let home_dir = fixtures().join("home/skills").display().to_string();
    assert_eq!(resolved[0]["dirs"][0]["path"], json!(home_dir));
    let workspace = fs::canonicalize(case.workspace()).expect("workspace");
    assert_eq!(
        resolved[0]["dirs"][2]["path"],
        json!(format!("{}/skills", workspace.display()))
    );

    // Pebble's discovery and activation, attributed to the node.
    let envelopes = persisted_events(&case.run_dir, "pebble");
    let discovered: Vec<&Value> = envelopes
        .iter()
        .filter(|e| e["event"]["event"].get("SkillsDiscovered").is_some())
        .collect();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    assert_eq!(discovered[0]["node"], "agent");
    let names: Vec<&str> = discovered[0]["event"]["event"]["SkillsDiscovered"]["skills"]
        .as_array()
        .expect("skills")
        .iter()
        .map(|s| s["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, [
        "cleanup",
        "greet",
        "home-only",
        "project-only",
        "repo-only"
    ]);
    let activated: Vec<&Value> = envelopes
        .iter()
        .filter(|e| e["event"]["event"].get("SkillActivated").is_some())
        .collect();
    assert_eq!(activated.len(), 1, "{activated:?}");
    assert_eq!(
        activated[0]["event"]["event"]["SkillActivated"],
        json!({ "skill_name": "greet", "source": "tool" })
    );

    // The broken files: a warning event each, and a terminal line each.
    let warnings = persisted_events(&case.run_dir, "fabro.skills.warning");
    let mut skipped: Vec<String> = warnings
        .iter()
        .map(|w| {
            Path::new(w["path"].as_str().expect("path"))
                .parent()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .expect("dir")
        })
        .collect();
    skipped.sort();
    assert_eq!(skipped, ["no-frontmatter", "no-name", "unterminated"]);
    assert!(
        warnings.iter().all(|w| w["reason"] == "malformed"),
        "{warnings:?}"
    );
    let echoed = finished.echoed();
    let terminal: Vec<&String> = echoed
        .iter()
        .filter(|(node, line)| node == "agent" && line.starts_with("skills: "))
        .map(|(_, line)| line)
        .collect();
    assert_eq!(terminal.len(), 3, "{echoed:?}");
    assert!(
        terminal
            .iter()
            .any(|line| line.contains("no-name/SKILL.md") && line.contains("name")),
        "{terminal:?}"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// The Claude 5 vocabulary: the skill tool is `Skill` with `skill` and
/// `args`, exactly as the reference defines it, and the arguments reach the
/// template's placeholder.
#[tokio::test]
async fn the_claude_vocabulary_offers_the_reference_skill_tool() {
    let provider = Provider::Anthropic;
    let mut case = Case::new("skills-claude");
    let model = model(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "load",
            model,
            "Greet Ada",
            tool_call("load", "Skill", json!({ "skill": "greet", "args": "Ada" })),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model,
            "REPOSITORY GREETING: Ada",
            text("Greeted Ada."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Skills {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    {prepare}
    agent [prompt="Greet Ada with the greet skill.", model="{model}", provider="anthropic", on_failure="exit"]
    start -> prepare -> agent -> exit
}}"#,
            prepare = prepare_repository(),
        ),
        None,
    );
    let finished = case.run_with(&workflow, &[], launch_with_home()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["load", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    let system = system_text(&requests[0]);
    let section = expected("prompt-section.Skill.txt");
    assert!(
        system.contains(section.trim_end()),
        "the skills section names the `Skill` tool:\n{system}"
    );
    assert!(
        tool_named(&requests[0], "use_skill").is_none(),
        "no canonical name on the Claude wire"
    );
    let tool = tool_named(&requests[0], "Skill").expect("the Skill tool is offered");
    let reference = expected_json("tool.Skill.json");
    assert_eq!(tool["description"], reference["description"]);
    assert_eq!(tool["input_schema"], reference["parameters"], "{tool}");
    let second = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        second.contains("REPOSITORY GREETING: Ada"),
        "the args filled the placeholder: {second}"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Greeted Ada.")
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A skill that tells the model to delete a file: the model loads it, then
/// calls the shell as instructed, and the configured `pre_tool_use` hook
/// blocks that call exactly as it blocks any other. The post hook logs the
/// one call that ran (the skill load itself).
#[tokio::test]
async fn a_skill_driven_tool_call_is_still_subject_to_tool_hooks() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("skills-hooks");
    let model = model(provider);
    let shell = shell_tool(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "load",
            model,
            "Clean up the workspace",
            tool_call("load", "use_skill", json!({ "skill_name": "cleanup" })),
        ),
        scenario(
            provider,
            &case.credential,
            "destroy",
            model,
            "CLEANUP INSTRUCTIONS",
            tool_call(
                "destroy",
                shell,
                json!({ "command": "rm -f important.txt && echo REMOVED" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model,
            "destructive commands are not allowed",
            text("Left important.txt alone."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Skills {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    {prepare}
    agent [prompt="Clean up the workspace with the cleanup skill.", model="{model}", provider="openai", on_failure="exit"]
    verify [shape=parallelogram, script="cat important.txt tool-hooks.log"]
    start -> prepare -> agent -> verify -> exit
}}"#,
            prepare = prepare_repository(),
        ),
        Some(
            r#"
[[run.hooks]]
name = "no-destruction"
event = "pre_tool_use"
matcher = "shell|Bash"
script = "if grep -q 'rm ' \"$FABRO_HOOK_CONTEXT\"; then echo '{\"decision\":\"block\",\"reason\":\"destructive commands are not allowed\"}'; exit 2; fi"

[[run.hooks]]
name = "log-tools"
event = "post_tool_use"
script = "echo ran:$FABRO_NODE_ID >> tool-hooks.log"
"#,
        ),
    );
    let finished = case.run_with(&workflow, &[], launch_with_home()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["load", "destroy", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let workspace = case.workspace();
    assert_eq!(
        fs::read_to_string(workspace.join("important.txt")).expect("important.txt"),
        "precious\n",
        "the skill-driven rm never ran"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("tool-hooks.log")).expect("tool-hooks.log"),
        "ran:agent\n",
        "the post hook saw the one call that ran: the skill load"
    );
    let requests = twin.requests_for(&case.credential);
    let third = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        third.contains("destructive commands are not allowed"),
        "the model saw the block reason: {third}"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Left important.txt alone.")
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A skill loaded in one `for_each` branch stays in that branch's session:
/// the sibling's requests and the node after the join never see the
/// template, and every session records its own directory resolution.
#[tokio::test]
async fn a_skill_loaded_in_one_branch_does_not_reach_its_sibling() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("skills-branches");
    let model = model(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "alpha-load",
            model,
            r#""name": "alpha""#,
            tool_call("load", "use_skill", json!({ "skill_name": "greet" })),
        ),
        scenario(
            provider,
            &case.credential,
            "alpha-answer",
            model,
            "REPOSITORY GREETING",
            text("alpha done"),
        ),
        scenario(
            provider,
            &case.credential,
            "beta",
            model,
            r#""name": "beta""#,
            text("beta done"),
        ),
        scenario(
            provider,
            &case.credential,
            "wrap",
            model,
            "Wrap up",
            text("wrapped"),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Skills {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    {prepare}
    plan [shape=parallelogram, output_schema="routing", script="printf '%s' '{{\"context_updates\":{{\"jobs\":[{{\"name\":\"alpha\"}},{{\"name\":\"beta\"}}]}}}}'"]
    fan [shape=component, for_each="context.jobs", max_parallel=2]
    job [prompt="Handle the item; greet it with the greet skill when it is alpha.", model="{model}", provider="openai"]
    join [shape=tripleoctagon]
    after [prompt="Wrap up.", model="{model}", provider="openai"]
    start -> prepare -> plan -> fan -> job -> join -> after -> exit
}}"#,
            prepare = prepare_repository(),
        ),
        None,
    );
    let finished = case.run_with(&workflow, &[], launch_with_home()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let mut consumed = twin.consumed();
    consumed.sort();
    assert_eq!(consumed, ["alpha-answer", "alpha-load", "beta", "wrap"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 4, "{requests:?}");
    let texts: Vec<String> = requests
        .iter()
        .map(|r| serde_json::to_string(r).expect("request"))
        .collect();
    let with_template: Vec<&String> = texts
        .iter()
        .filter(|t| t.contains("REPOSITORY GREETING"))
        .collect();
    assert_eq!(
        with_template.len(),
        1,
        "only alpha's second request carries the loaded skill: {texts:#?}"
    );
    assert!(
        with_template[0].contains(r#"\"name\": \"alpha\""#)
            || with_template[0].contains(r#""name": "alpha""#),
        "{}",
        with_template[0]
    );
    let beta = texts
        .iter()
        .find(|t| t.contains(r#"\"name\": \"beta\""#) || t.contains(r#""name": "beta""#))
        .expect("beta's request");
    assert!(!beta.contains("REPOSITORY GREETING"), "{beta}");
    assert!(
        !beta.contains("\"call_id\""),
        "beta made no tool call: {beta}"
    );
    let wrap = texts
        .iter()
        .find(|t| t.contains("Wrap up"))
        .expect("the wrap-up request");
    assert!(!wrap.contains("REPOSITORY GREETING"), "{wrap}");
    // Every session's skills section still lists `greet`: discovery is per
    // session, and only activation is scoped.
    for request in &requests {
        assert!(
            system_text(request).contains("- `greet`: Greet from the repository skills directory"),
            "{request}"
        );
    }
    let resolved = persisted_events(&case.run_dir, "fabro.skills");
    let mut nodes: Vec<&str> = resolved
        .iter()
        .map(|e| e["node"].as_str().expect("node"))
        .collect();
    nodes.sort_unstable();
    assert_eq!(nodes, ["after", "job", "job"]);
    let activated = persisted_events(&case.run_dir, "pebble")
        .into_iter()
        .filter(|e| e["event"]["event"].get("SkillActivated").is_some())
        .collect::<Vec<_>>();
    assert_eq!(activated.len(), 1, "{activated:?}");
    assert_eq!(activated[0]["node"], "job");
    finished.assert_no_leaked_processes().await;
    twin.stop();
}
