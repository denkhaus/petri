//! Handoff §7 test 9: real corpus workflows, run end to end on the executor.
//!
//! Both workflows drive `gh`. Running them for real would call GitHub's API
//! with the machine's credentials, so `gh` is a stub on `PATH` that records its
//! invocations and answers the one query the scripts make. Everything else —
//! bash, the outputs file, env layering, `if:` gates over the `github` context,
//! job summaries — is the real thing, on real processes through
//! `runtime::Runtime`, with replay verified byte for byte by the runtime
//! itself.
//!
//! The corpus is fetched, not committed, so these skip when it is absent. See
//! `acceptance::has_corpus` for the convention.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, process};

use frontend_gha::load;
use runtime::driver::ExecutionReport;
use runtime::executor::{MapSecrets, Retention};
use runtime::frontend::DirFiles;
use runtime::ir::{Graph, RunStatus};
use runtime::{RunOptions, Runtime, engine, ir};
use serde_json::json;
use support::install_gh_stub;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

/// Is this corpus repo fetched? These tests name specific files, so it is that
/// repo's workflows that have to be on disk, not merely some of the corpus.
///
/// `PETRI_REQUIRE_CORPUS` turns the skip into a failure; CI sets it.
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr; a test binary has no other sink"
)]
fn is_corpus_repo_ready(repo: &str) -> bool {
    if corpus_root().join(repo).join(".github/workflows").is_dir() {
        return true;
    }
    assert!(
        !env::var("PETRI_REQUIRE_CORPUS").is_ok_and(|v| !v.is_empty()),
        "PETRI_REQUIRE_CORPUS is set, but the corpus is not fetched ({repo} is missing)"
    );
    eprintln!("skipping: corpus not fetched; run scripts/corpus-fetch.sh");
    false
}

#[expect(
    clippy::print_stderr,
    reason = "the lowering diagnostics belong on stderr, where a failing corpus test shows them"
)]
fn lower(repo: &str, workflow: &str) -> Graph {
    let root = corpus_root().join(repo);
    let file = root.join(".github/workflows").join(workflow);
    let text = fs::read_to_string(&file).expect("corpus workflow");
    let lowered = load(&format!(".github/workflows/{workflow}"), &text, &DirFiles {
        root,
    });
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("the workflow lowers")
}

/// Point every scope's `PATH` at the stub, and give the run its `github`
/// context.
fn prepare(mut graph: Graph, bin: &Path, stub_log: &Path, github: serde_json::Value) -> Graph {
    let path = format!(
        "{}:{}",
        bin.display(),
        env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    for scope in &mut graph.body.scopes {
        scope
            .env
            .insert("PATH".into(), ir::ExprOrValue::Value(json!(path)));
        scope.env.insert(
            "GH_STUB_LOG".into(),
            ir::ExprOrValue::Value(json!(stub_log.display().to_string())),
        );
    }
    graph.params.insert("github".into(), github);
    graph.params.insert(
        "runner".into(),
        json!({ "os": env::consts::OS, "arch": env::consts::ARCH, "name": "local" }),
    );
    graph.params.insert("vars".into(), json!({}));
    graph
}

async fn run(graph: Graph, dir: &Path) -> ExecutionReport {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    let rt = Runtime::standard()
        .step(github_actions::RunStep)
        .step(github_actions::ActionStep)
        .step(github_actions::DeferredActionStep)
        .step(github_actions::DeferredActionResultStep)
        .step(github_actions::DeferredActionPublishStep)
        .step(github_actions::DeferredActionPostStep)
        .secrets(MapSecrets::from_pairs(&[(
            "GITHUB_TOKEN",
            "ghs_dummy_token_for_the_stub_0000",
        )]))
        .options(options);
    rt.run(graph).await.expect("replay is byte-identical")
}

fn fresh_dir(label: &str) -> PathBuf {
    let dir = env::temp_dir()
        .join("petri-corpus")
        .join(format!("{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the temp dir for this run is creatable");
    dir
}

/// facebook/react `shared_cleanup_stale_branch_caches.yml`: one job, one `run:`
/// step that lists caches with `gh` and deletes each. With the stub answering
/// two cache ids, the script deletes both.
#[tokio::test]
async fn react_cleanup_stale_branch_caches_runs() {
    if !is_corpus_repo_ready("facebook__react") {
        return;
    }
    let dir = fresh_dir("react-cleanup");
    let bin = install_gh_stub(&dir);
    let stub_log = dir.join("gh.log");
    let graph = lower("facebook__react", "shared_cleanup_stale_branch_caches.yml");
    let graph = prepare(
        graph,
        &bin,
        &stub_log,
        json!({ "repository": "facebook/react", "event_name": "schedule", "actor": "bot", "sha": "abc", "ref": "refs/heads/main" }),
    );

    let report = run(graph, &dir).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let calls = fs::read_to_string(&stub_log).unwrap_or_default();
    assert!(calls.contains("gh cache list"), "{calls}");
    assert!(calls.contains("gh cache delete 101"), "{calls}");
    assert!(calls.contains("gh cache delete 202"), "{calls}");
    // The job summary is on the record.
    assert_eq!(
        report
            .state
            .run_context()
            .node("cleanup/done")
            .unwrap()
            .output["result"],
        json!("success")
    );
    // Env layering reached the process: the secret was in scope (and masked).
    let log: String = report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgress {
                ev: ir::StepEvent::Log { line, .. },
                ..
            } => Some(line.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(log.contains("Done"), "{log}");
    assert!(
        !serde_json::to_string(&report.state)
            .unwrap()
            .contains("ghs_dummy_token"),
        "the secret stayed out of the state"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// nodejs/node `comment-labeled.yml`: three jobs gated on
/// `github.event.label.name`. With the label set to `stalled`, exactly one runs
/// and posts through `gh`; the other two are skipped by their `if:`.
#[tokio::test]
async fn nodejs_comment_labeled_runs_the_matching_job() {
    if !is_corpus_repo_ready("nodejs__node") {
        return;
    }
    let dir = fresh_dir("node-comment-labeled");
    let bin = install_gh_stub(&dir);
    let stub_log = dir.join("gh.log");
    let graph = lower("nodejs__node", "comment-labeled.yml");
    let graph = prepare(
        graph,
        &bin,
        &stub_log,
        json!({
            "repository": "nodejs/node", "event_name": "issues", "actor": "someone",
            "event": { "label": { "name": "stalled" }, "issue": { "number": 4242 } },
        }),
    );

    let report = run(graph, &dir).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let calls = fs::read_to_string(&stub_log).unwrap_or_default();
    assert!(
        calls.contains("gh issue comment 4242 --repo nodejs/node"),
        "the stalled job ran with the issue number from the event: {calls}"
    );
    // One invocation; the body it posted spans lines, so count invocations, not
    // lines.
    let invocations = calls.lines().filter(|l| l.starts_with("gh ")).count();
    assert_eq!(
        invocations,
        1,
        "the other two jobs were gated off: {:?}",
        calls.lines().collect::<Vec<_>>()
    );

    let ctx = report.state.run_context();
    assert_eq!(
        ctx.node("stale-comment/done").unwrap().output["result"],
        json!("success")
    );
    assert_eq!(
        ctx.node("fast-track/done").unwrap().output["result"],
        json!("skipped")
    );
    assert_eq!(
        ctx.node("notable-change/done").unwrap().output["result"],
        json!("skipped")
    );
    let _ = fs::remove_dir_all(&dir);
}
