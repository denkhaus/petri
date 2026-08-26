//! Handoff §7 test 9: real corpus workflows, run end to end on the executor.
//!
//! Both workflows drive `gh`. Running them for real would call GitHub's API with the
//! machine's credentials, so `gh` is a stub on `PATH` that records its invocations
//! and answers the one query the scripts make. Everything else — bash, the outputs
//! file, env layering, `if:` gates over the `github` context, job summaries — is the
//! real thing, on real processes, with replay verified byte for byte.

use std::sync::Arc;
use std::time::Duration;

use driver::{Driver, RunConfig};
use executor::{HostExecutor, MapSecrets, Retention};
use ir::{Graph, RunStatus};
use serde_json::json;

fn corpus_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

fn lower(repo: &str, workflow: &str) -> Graph {
    let root = corpus_root().join(repo);
    let file = root.join(".github/workflows").join(workflow);
    let text = std::fs::read_to_string(&file).expect("vendored workflow");
    let lowered = frontend_gha::load(
        &format!(".github/workflows/{workflow}"),
        &text,
        &frontend_gha::DirFiles { root },
    );
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("the workflow lowers")
}

/// A `gh` that records what it was asked and answers `cache list`.
fn install_gh_stub(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("gh");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
echo "gh $*" >> "$GH_STUB_LOG"
case "$1 $2" in
  "cache list") echo 101; echo 202 ;;
esac
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Point every scope's `PATH` at the stub, and give the run its `github` context.
fn prepare(
    mut graph: Graph,
    bin: &std::path::Path,
    stub_log: &std::path::Path,
    github: serde_json::Value,
) -> Graph {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    for scope in &mut graph.scopes {
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
        json!({ "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "name": "local" }),
    );
    graph.params.insert("vars".into(), json!({}));
    graph
}

async fn run(graph: Graph, dir: &std::path::Path) -> driver::RunReport {
    let executor: Arc<dyn executor::Executor> =
        Arc::new(HostExecutor::new(dir).with_retention(Retention::Never));
    let mut runners = steps::RunnerRegistry::new();
    runners.register(Arc::new(steps::ProcessStep));
    runners.register(Arc::new(steps::NoopStep));
    let secrets = MapSecrets::from_pairs(&[("GITHUB_TOKEN", "ghs_dummy_token_for_the_stub_0000")]);
    Driver::new(
        graph,
        executor,
        runners,
        Arc::new(secrets),
        RunConfig::new(dir).with_grace(Duration::from_secs(2)),
    )
    .run()
    .await
}

fn fresh_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("petri-corpus")
        .join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// facebook/react `shared_cleanup_stale_branch_caches.yml`: one job, one `run:`
/// step that lists caches with `gh` and deletes each. With the stub answering two
/// cache ids, the script deletes both.
#[tokio::test]
async fn react_cleanup_stale_branch_caches_runs() {
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

    let report = run(graph.clone(), &dir).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    engine::verify_replay(graph, &report.state.log).expect("replay is byte-identical");

    let calls = std::fs::read_to_string(&stub_log).unwrap_or_default();
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// nodejs/node `comment-labeled.yml`: three jobs gated on `github.event.label.name`.
/// With the label set to `stalled`, exactly one runs and posts through `gh`; the
/// other two are skipped by their `if:`.
#[tokio::test]
async fn nodejs_comment_labeled_runs_the_matching_job() {
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

    let report = run(graph.clone(), &dir).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    engine::verify_replay(graph, &report.state.log).expect("replay is byte-identical");

    let calls = std::fs::read_to_string(&stub_log).unwrap_or_default();
    assert!(
        calls.contains("gh issue comment 4242 --repo nodejs/node"),
        "the stalled job ran with the issue number from the event: {calls}"
    );
    // One invocation; the body it posted spans lines, so count invocations, not lines.
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
    let _ = std::fs::remove_dir_all(&dir);
}
