//! Readiness item 9c on real scopes: Fabro's skill directories and
//! precedence resolved by Petri, discovered and served by Pebble, against
//! the versioned fixtures in `crates/fabro/acceptance/testdata/skills`.
//!
//! A scripted model drives the native backend on the host executor; the
//! workspace is a Git repository holding the `repo` fixture and the
//! `FabroHome` capability points at the `home` fixture.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs};

use fabro_steps::pebble::PebbleClient;
use fabro_steps::register;
use fabro_steps::skills::{FabroHome, MISSING_CLASS, RESOLVED_EVENT, WARNING_EVENT};
use frontend::{CompileInputs, MapFiles};
use ir::{Graph, RunStatus, StepEvent, Value};
use lithos_llm::types::{Role, ToolDefinition};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedProvider, message_text, scripted_client, text_response,
    tool_call_response,
};
use runtime::driver::{EventObserver, ExecutionReport};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};

/// Every `StepEvent::Custom` the run emitted, with the node it came from.
#[derive(Default)]
struct Customs(Mutex<Vec<(String, Value)>>);

impl EventObserver for Customs {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, state: &EngineState) {
        if let Event::StepProgressRecorded {
            firing,
            ev: StepEvent::Custom(value),
        } = &record.event
        {
            let node = state
                .firing_node(*firing)
                .and_then(|id| state.graph().node(id))
                .map(|n| n.name.to_string())
                .unwrap_or_default();
            self.0
                .lock()
                .expect("not poisoned")
                .push((node, value.clone()));
        }
    }
}

impl Customs {
    fn of_kind(&self, kind: &str) -> Vec<(String, Value)> {
        self.0
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|(_, value)| value["kind"] == kind)
            .cloned()
            .collect()
    }

    /// Pebble events of one variant, with the node they came from.
    fn pebble(&self, variant: &str) -> Vec<(String, Value)> {
        self.of_kind("pebble")
            .into_iter()
            .filter_map(|(node, envelope)| {
                envelope["event"]["event"]
                    .get(variant)
                    .cloned()
                    .map(|event| (node, event))
            })
            .collect()
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../acceptance/testdata/skills")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("target dir");
    for entry in fs::read_dir(from).expect("fixture dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/scope-0/work")
}

/// The `repo` fixture as a Git repository in the run's workspace, or `None`
/// when Git is unavailable.
fn repository(dir: &RunDir) -> Option<PathBuf> {
    let ws = workspace(dir);
    copy_tree(&fixtures().join("repo"), &ws);
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status();
    git.is_ok_and(|s| s.success()).then_some(ws)
}

fn lower(dot: &str, toml: &str) -> Graph {
    let files = MapFiles(BTreeMap::from([(
        "wf/workflow.toml".to_string(),
        toml.to_string(),
    )]));
    let lowered = frontend_fabro::load("wf/w.fabro", dot, &files, &CompileInputs::new());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    lowered.graph.expect("lowers")
}

fn one_agent(prompt: &str, toml: &str) -> Graph {
    lower(
        &format!(
            r#"digraph W {{
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [prompt="{prompt}", on_failure="exit"]
        start -> a -> exit
    }}"#
        ),
        toml,
    )
}

async fn run(
    dir: &RunDir,
    graph: Graph,
    calls: Vec<ScriptedCall>,
    home: Option<PathBuf>,
) -> (ExecutionReport, Arc<Customs>, Arc<ScriptedProvider>) {
    let (client, provider) = scripted_client(calls);
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    let customs = Arc::new(Customs::default());
    // An explicit home always: the developer's own `~/.fabro/skills` must
    // never reach a test.
    let home = home.unwrap_or_else(|| dir.path().join("no-home"));
    let rt = register(
        Runtime::standard()
            .observe(customs.clone())
            .options(options)
            .capability(PebbleClient(client))
            .capability(FabroHome(home)),
    );
    let report = rt.run(graph).await.expect("replay is byte-identical");
    (report, customs, provider)
}

fn system_text(provider: &ScriptedProvider) -> String {
    provider.requests()[0]
        .messages()
        .iter()
        .filter(|m| m.role() == Role::System)
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_text(provider: &ScriptedProvider, index: usize) -> String {
    provider.requests()[index]
        .messages()
        .iter()
        .filter(|m| m.role() == Role::User)
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn skill_tool(provider: &ScriptedProvider) -> Option<ToolDefinition> {
    provider.requests()[0]
        .tools()
        .iter()
        .find(|tool| tool.name == "use_skill")
        .cloned()
}

/// Precedence and prompt: the configured directory, `.fabro/skills` and
/// `skills` are searched in Fabro's order, the repository's `greet` wins,
/// the prompt section and the tool match the versioned reference, and the
/// tool serves the winning template.
#[tokio::test]
async fn directories_resolve_in_fabros_order_and_the_repository_wins() {
    let dir = RunDir::new("skills-order");
    let Some(ws) = repository(&dir) else {
        return;
    };
    let home = fixtures().join("home");
    let (report, customs, provider) = run(
        &dir,
        one_agent("Greet Ada with the greet skill.", ""),
        vec![
            ScriptedCall::response(tool_call_response(
                "use_skill",
                "load",
                json!({"skill_name": "greet"}),
            )),
            ScriptedCall::response(text_response("Greeted.")),
        ],
        Some(home.clone()),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "Greeted.");

    // Petri's own record of the directories, attributed to the node.
    let resolved = customs.of_kind(RESOLVED_EVENT);
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0].0, "a");
    assert_eq!(resolved[0].1["node"], "a");
    assert!(!resolved[0].1["firing"].is_null());
    let root = fs::canonicalize(&ws)
        .expect("workspace")
        .display()
        .to_string();
    let expected_dirs = json!([
        {"path": home.join("skills").display().to_string(), "source": "configured"},
        {"path": format!("{root}/.fabro/skills"), "source": "project_fabro"},
        {"path": format!("{root}/skills"), "source": "project"},
    ]);
    assert_eq!(resolved[0].1["dirs"], expected_dirs, "{:#}", resolved[0].1);

    // Pebble searched the same list, in order, and the later name won.
    let discovered = customs.pebble("SkillsDiscovered");
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    assert_eq!(discovered[0].0, "a");
    let source_dirs: Vec<String> = expected_dirs
        .as_array()
        .expect("list")
        .iter()
        .map(|d| d["path"].as_str().expect("path").to_owned())
        .collect();
    assert_eq!(discovered[0].1["source_dirs"], json!(source_dirs));
    let names: Vec<(String, String)> = discovered[0].1["skills"]
        .as_array()
        .expect("skills")
        .iter()
        .map(|s| {
            (
                s["name"].as_str().expect("name").to_owned(),
                s["description"].as_str().expect("description").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("cleanup", "Remove scratch files from the workspace"),
            ("greet", "Greet from the repository skills directory"),
            ("home-only", "Only in the Fabro home directory"),
            ("project-only", "Only in the project .fabro directory"),
            ("repo-only", "Only in the repository skills directory"),
        ]
        .map(|(n, d)| (n.to_owned(), d.to_owned()))
    );

    // The reference prompt section and tool definition.
    let section = fs::read_to_string(fixtures().join("expected/prompt-section.use_skill.txt"))
        .expect("expected section");
    let system = system_text(&provider);
    assert!(
        system.contains(section.trim_end()),
        "the skills section is the reference text:\n{system}"
    );
    let expected_tool: Value = serde_json::from_str(
        &fs::read_to_string(fixtures().join("expected/tool.use_skill.json")).expect("tool"),
    )
    .expect("tool json");
    let tool = skill_tool(&provider).expect("the use_skill tool is offered");
    assert_eq!(
        tool,
        ToolDefinition::function(
            expected_tool["name"].as_str().expect("name"),
            expected_tool["description"].as_str().expect("description"),
            expected_tool["parameters"].clone(),
        )
    );

    // The tool returned the winning template, and the next request carries it.
    let second = serde_json::to_string(&provider.requests()[1]).expect("request");
    assert!(second.contains("REPOSITORY GREETING"), "{second}");
    assert!(!second.contains("PROJECT GREETING"), "{second}");
    assert!(!second.contains("HOME GREETING"), "{second}");
    let activated = customs.pebble("SkillActivated");
    assert_eq!(activated.len(), 1, "{activated:?}");
    assert_eq!(activated[0].0, "a");
    assert_eq!(activated[0].1["skill_name"], "greet");
    assert_eq!(activated[0].1["source"], "tool");

    // The three broken files are diagnosed, never skipped in silence.
    let warnings = customs.of_kind(WARNING_EVENT);
    let mut skipped: Vec<(String, String)> = warnings
        .iter()
        .map(|(_, w)| {
            (
                Path::new(w["path"].as_str().expect("path"))
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .expect("dir"),
                w["reason"].as_str().expect("reason").to_owned(),
            )
        })
        .collect();
    skipped.sort();
    assert_eq!(skipped, [
        ("no-frontmatter".to_owned(), "malformed".to_owned()),
        ("no-name".to_owned(), "malformed".to_owned()),
        ("unterminated".to_owned(), "malformed".to_owned()),
    ]);
    assert!(
        warnings
            .iter()
            .all(|(node, w)| node == "a" && w["node"] == "a" && !w["firing"].is_null()),
        "{warnings:?}"
    );
}

/// A `/name` reference in the prompt expands to the winning skill's
/// template before the model sees it (Fabro and Pebble expand the first
/// input); a workflow-named directory overrides the repository's.
#[tokio::test]
async fn a_slash_reference_in_the_prompt_expands_to_the_selected_skill() {
    let dir = RunDir::new("skills-slash");
    if repository(&dir).is_none() {
        return;
    }
    let extra = fixtures().join("workflow/skills");
    let toml = format!(
        "_version = 1\n[run.agent]\nskills = [\"{}\"]\n",
        extra.display()
    );
    let (report, customs, provider) = run(
        &dir,
        one_agent("/greet Ada", &toml),
        vec![ScriptedCall::response(text_response("Hello Ada."))],
        Some(fixtures().join("home")),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let user = user_text(&provider, 0);
    assert!(
        user.contains("WORKFLOW GREETING:") && user.contains("Ada"),
        "the workflow's greet won and took the input: {user}"
    );
    assert!(!user.contains("/greet"), "{user}");
    assert!(!user.contains("REPOSITORY GREETING"), "{user}");
    let activated = customs.pebble("SkillActivated");
    assert_eq!(activated.len(), 1, "{activated:?}");
    assert_eq!(activated[0].1["source"], "slash");
    let resolved = customs.of_kind(RESOLVED_EVENT);
    let last = resolved[0].1["dirs"]
        .as_array()
        .expect("dirs")
        .last()
        .cloned()
        .expect("a workflow dir");
    assert_eq!(last["source"], "workflow");
    assert_eq!(last["path"], json!(extra.display().to_string()));
    let system = system_text(&provider);
    assert!(
        system.contains("- `greet`: Greet from the workflow's own skills directory"),
        "{system}"
    );
}

/// A prompt naming a skill the session does not have fails the node with
/// its own class; a workflow-named directory that does not exist is
/// reported; a session with no skills offers no skill tool and no section.
#[tokio::test]
async fn missing_skills_and_directories_are_diagnosed() {
    let dir = RunDir::new("skills-missing");
    if repository(&dir).is_none() {
        return;
    }
    let (report, customs, provider) = run(
        &dir,
        one_agent("/nope do the thing", ""),
        vec![ScriptedCall::response(text_response("never"))],
        Some(fixtures().join("home")),
    )
    .await;
    assert_eq!(report.status, RunStatus::Failed, "{:?}", report.status);
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    let outcome = report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .expect("agent outcome")
        .outcome
        .clone();
    let text = serde_json::to_string(&outcome).expect("outcome");
    assert!(text.contains(MISSING_CLASS), "{text}");
    assert!(text.contains("Unknown skill: /nope"), "{text}");
    assert!(provider.requests().is_empty(), "the model was never called");
    assert!(customs.pebble("SkillActivated").is_empty());

    let dir = RunDir::new("skills-missing-dir");
    let (report, customs, provider) = run(
        &dir,
        one_agent(
            "Say hi.",
            "_version = 1\n[run.agent]\nskills = [\"nowhere/skills\"]\n",
        ),
        vec![ScriptedCall::response(text_response("hi"))],
        None,
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let warnings = customs.of_kind(WARNING_EVENT);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].1["reason"], "missing_directory");
    assert!(
        warnings[0].1["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("/nowhere/skills")),
        "{:?}",
        warnings[0].1
    );
    assert!(
        skill_tool(&provider).is_none(),
        "no skills, no skill tool: {:?}",
        provider.requests()[0].tools()
    );
    assert!(!system_text(&provider).contains("# Available Skills"));
    let discovered = customs.pebble("SkillsDiscovered");
    assert_eq!(discovered[0].1["skills"], json!([]));
}
