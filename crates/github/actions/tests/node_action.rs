//! A JavaScript action in a local git repository, run for real: the frontend
//! resolves `uses: acme/hello@v1` through a `GitActionSource` pointed at a `file://`
//! base, the step stages the tree and runs `node`, and the runner contract —
//! inputs, outputs, `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STATE`, `::add-mask::`,
//! `post` — is exercised end to end on the standard runtime, replay verified.
//!
//! Needs `git` and `node` on this machine; skips otherwise.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use frontend::NoFiles;
use frontend_gha::load_with;
use github_actions::{
    ActionRef, ActionSourceCap, ActionStep, ActionTreeSource, GitActionSource, RunStep,
};
use runtime::executor::{MapSecrets, Retention};
use runtime::ir::{Graph, RunStatus};
use runtime::{RunOptions, Runtime, engine, ir};
use serde_json::json;

const ACTION_YML: &str = r#"
name: Hello
description: A fixture action.
inputs:
  name:
    description: Who to greet.
    required: true
  shout:
    description: Upper-case the greeting.
    default: 'false'
runs:
  using: node20
  main: dist/index.js
  post: dist/post.js
"#;

const INDEX_JS: &str = r#"
const fs = require('fs');
const name = process.env.INPUT_NAME;
let greeting = `Hello, ${name}`;
if (process.env.INPUT_SHOUT === 'true') greeting = greeting.toUpperCase();
fs.appendFileSync(process.env.GITHUB_OUTPUT, `greeting=${greeting}\n`);
fs.appendFileSync(process.env.GITHUB_ENV, `HELLO_ENV=from-action\nMULTI<<EOF\nline one\nline two\nEOF\n`);
const bin = process.env.RUNNER_TEMP + '/bin';
fs.mkdirSync(bin, { recursive: true });
fs.writeFileSync(bin + '/hellotool', '#!/bin/sh\necho hellotool-ran\n', { mode: 0o755 });
fs.appendFileSync(process.env.GITHUB_PATH, bin + '\n');
fs.appendFileSync(process.env.GITHUB_STATE, 'token=abc123\n');
console.log('::save-state name=saved::yes');
console.log('::add-mask::supersecretvalue');
console.log('the value is supersecretvalue');
console.log('::warning::heads up');
console.log('cwd=' + process.cwd());
console.log('action_path_ok=' + fs.existsSync(process.env.GITHUB_ACTION_PATH + '/action.yml'));
console.log('action_ref=' + process.env.GITHUB_ACTION_REPOSITORY + '@' + process.env.GITHUB_ACTION_REF);
console.log('event_name=' + JSON.parse(fs.readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8')).action);
"#;

const POST_JS: &str = r#"
console.log('post saw ' + process.env.STATE_token + ' ' + process.env.STATE_saved);
"#;

const WORKFLOW: &str = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: hello
        uses: acme/hello@v1
        with:
          name: World
      - env:
          AUTH: token ${{ github.token }}
        run: |
          echo "greeting=${{ steps.hello.outputs.greeting }}"
          echo "env=$HELLO_ENV"
          echo "multi=$MULTI"
          hellotool
          echo "auth=$AUTH"
          echo "inline=${{ github.token }}"
          echo "summary line" >> "$GITHUB_STEP_SUMMARY"
      - run: echo "pwd=$(pwd)"
"#;

/// The run's `GITHUB_TOKEN`; long enough to be masked.
const FIXTURE_TOKEN: &str = "ghp_fixture_token_0123456789";

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// `<base>/acme/hello`, a repository whose tag `v1` is the fixture action.
fn fixture_action(base: &Path) {
    let dir = base.join("acme").join("hello");
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    std::fs::write(dir.join("action.yml"), ACTION_YML).unwrap();
    std::fs::write(dir.join("dist/index.js"), INDEX_JS).unwrap();
    std::fs::write(dir.join("dist/post.js"), POST_JS).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(
        &dir,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-q",
            "-m",
            "the action",
        ],
    );
    git(&dir, &["tag", "v1"]);
}

fn with_params(mut graph: Graph) -> Graph {
    graph.params.insert(
        "github".into(),
        json!({
            "repository": "example/repo", "event_name": "push", "actor": "tester",
            "ref": "refs/heads/main", "ref_name": "main", "sha": "0123456789abcdef",
            "run_id": "1", "run_number": "1",
            "event": { "action": "opened" },
        }),
    );
    graph.params.insert(
        "runner".into(),
        json!({ "os": "Linux", "arch": "X64", "name": "local" }),
    );
    graph.params.insert("vars".into(), json!({}));
    graph
}

fn runtime(dir: &Path, source: &Arc<GitActionSource>) -> Runtime {
    let mut options = RunOptions::new(dir.join("run"));
    options.grace = Duration::from_secs(1);
    options.retention = Retention::Never;
    let trees: Arc<dyn ActionTreeSource> = source.clone();
    Runtime::standard()
        .options(options)
        .step(RunStep)
        .step(ActionStep)
        .capability(ActionSourceCap(trees))
        .secrets(MapSecrets::from_pairs(&[("GITHUB_TOKEN", FIXTURE_TOKEN)]))
}

#[tokio::test]
async fn a_javascript_action_runs_with_the_runner_contract() {
    if !have("git") || !have("node") {
        eprintln!("skipping: git and node are needed");
        return;
    }
    let run_dir = testkit::RunDir::new("gha-hello");
    let dir = run_dir.path();
    let remotes = dir.join("remotes");
    fixture_action(&remotes);
    let source = Arc::new(
        GitActionSource::new(dir.join("cache"))
            .with_remote_base(format!("file://{}", remotes.display())),
    );

    let lowered = load_with(
        ".github/workflows/ci.yml",
        WORKFLOW,
        &NoFiles,
        Some(source.as_ref()),
    );
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = with_params(lowered.graph.expect("the workflow lowers"));

    // The graph pins the commit the tag resolved to.
    let hello = graph
        .nodes
        .iter()
        .find(|n| n.name == "j/hello")
        .expect("the action node");
    let sha = hello.step.config["action"]["sha"]
        .as_str()
        .expect("a pinned sha")
        .to_string();
    assert_eq!(sha.len(), 40, "{sha}");

    let report = runtime(dir, &source)
        .run(graph)
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    // Order: main, the two run steps, then post at the end.
    let started = testkit::started(&report);
    let position = |name: &str| {
        started
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("{name} never started: {started:?}"))
    };
    assert!(position("j/hello") < position("j/step-2"));
    assert!(position("j/step-3") < position("j/hello/post"));

    let lines = testkit::log_lines(&report);
    let has = |want: &str| {
        assert!(
            lines.iter().any(|l| l == want),
            "no line `{want}` in {lines:#?}"
        );
    };
    // Inputs in, outputs out, and `steps.<id>.outputs` reads them.
    has("greeting=Hello, World");
    // GITHUB_ENV applies to later steps, heredoc form included.
    has("env=from-action");
    has("multi=line one");
    has("line two");
    // GITHUB_PATH applies to later steps.
    has("hellotool-ran");
    // `::add-mask::` masks from the next line on, and the command itself is gone.
    has("the value is ***");
    assert!(
        !lines.iter().any(|l| l.contains("supersecretvalue")),
        "the masked value leaked: {lines:#?}"
    );
    // `github.token` inside an env value and inside the script: the real value
    // reaches the process, the log sees only the mask.
    has("auth=token ***");
    has("inline=***");
    assert!(
        !lines.iter().any(|l| l.contains(FIXTURE_TOKEN)),
        "the token leaked: {lines:#?}"
    );
    // `::warning::` is rendered, not swallowed.
    has("Warning: heads up");
    // State from GITHUB_STATE and `::save-state::` reaches the post step.
    has("post saw abc123 yes");
    // The action sees its own files, its coordinates, and the event payload.
    has("action_path_ok=true");
    has("action_ref=acme/hello@v1");
    has("event_name=opened");
    // Every step runs in GITHUB_WORKSPACE, which is `<workspace>/repo`.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("cwd=") && l.ends_with("/repo")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("pwd=") && l.ends_with("/repo")),
        "{lines:#?}"
    );

    // The step summary rides out as a custom progress event.
    let summaries = report
        .state
        .log
        .events()
        .filter(|e| {
            matches!(
                e,
                engine::Event::StepProgress {
                    ev: ir::StepEvent::Custom(v),
                    ..
                } if v.get("github/step_summary").is_some()
            )
        })
        .count();
    assert_eq!(summaries, 1);

    // The record: outputs at the top level, state under its key.
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "j/hello")
        .expect("the action recorded");
    assert_eq!(record.outcome.output["greeting"], "Hello, World");
    assert_eq!(
        record.outcome.output[github_actions::STATE_OUTPUT_KEY],
        json!({ "token": "abc123", "saved": "yes" })
    );
}

#[test]
fn a_moving_reference_is_resolved_again_for_a_later_load() {
    if !have("git") {
        eprintln!("skipping: git is needed");
        return;
    }
    let run_dir = testkit::RunDir::new("gha-moving-ref");
    let remotes = run_dir.path().join("remotes");
    fixture_action(&remotes);
    let repo = remotes.join("acme/hello");
    git(&repo, &["branch", "-M", "main"]);
    let source = GitActionSource::new(run_dir.path().join("cache"))
        .with_remote_base(format!("file://{}", remotes.display()));
    let reference = ActionRef::parse("acme/hello@main").unwrap();
    let first = github_actions::ActionSource::resolve(&source, &reference).unwrap();

    std::fs::write(repo.join("dist/index.js"), "console.log('changed');\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-q",
            "-m",
            "move main",
        ],
    );

    let second = github_actions::ActionSource::resolve(&source, &reference).unwrap();
    assert_ne!(first.sha, second.sha);
}

/// `actions/checkout@v4` then `actions/setup-node@v4`, from GitHub, for real. Needs
/// the network and a `GITHUB_TOKEN` (or `gh auth token`); run with `--ignored`.
#[tokio::test]
#[ignore = "fetches real actions from github.com"]
async fn checkout_and_setup_node_run_for_real() {
    let token = std::env::var("GITHUB_TOKEN").ok().or_else(|| {
        Command::new("gh")
            .args(["auth", "token"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        panic!("this test needs GITHUB_TOKEN or a logged-in gh");
    };
    let run_dir = testkit::RunDir::new("gha-real");
    let dir = run_dir.path();
    let source = Arc::new(GitActionSource::new(dir.join("cache")));
    let workflow = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          repository: octocat/Hello-World
      - uses: actions/setup-node@v4
        with:
          node-version: '20'
      - run: |
          echo "node=$(node --version)"
          echo "readme=$(cat README)"
"#;
    let lowered = load_with(
        ".github/workflows/ci.yml",
        workflow,
        &NoFiles,
        Some(source.as_ref()),
    );
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = with_params(lowered.graph.expect("the workflow lowers"));
    let report = runtime(dir, &source)
        .secrets(MapSecrets::from_pairs(&[("GITHUB_TOKEN", &token)]))
        .run(graph)
        .await
        .expect("replay is byte-identical");
    let lines = testkit::log_lines(&report);
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}\n{lines:#?}",
        report.state.errors()
    );
    assert!(
        lines.iter().any(|l| l.starts_with("node=v20.")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l == "readme=Hello World!"),
        "{lines:#?}"
    );
}
