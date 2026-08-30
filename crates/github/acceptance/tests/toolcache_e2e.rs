//! Where `RUNNER_TOOL_CACHE` points, per environment: the host's persistent
//! store where the host registered one; the image's own populated cache where
//! the image names it (the runner images ship `/opt/hostedtoolcache` and say
//! so in their env — mounting an initially-empty persistent cache over it
//! would *remove* tools); the per-run workspace directory when neither exists.

mod support;

use std::path::PathBuf;
use std::{env, fs, process};

use acceptance::runs::RUNNER_IMAGE_2404;
use github_actions::ToolCacheCap;
use support::*;

fn workflow(container: Option<&str>) -> String {
    let container = container.map_or(String::new(), |image| format!("    container: {image}\n"));
    format!(
        "on: push\n\
         jobs:\n\
         \x20 probe:\n\
         \x20   runs-on: ubuntu-latest\n\
         {container}\
         \x20   steps:\n\
         \x20     - run: echo \"tool-cache=$RUNNER_TOOL_CACHE\"\n\
         \x20     - run: '[ \"${{{{ runner.tool_cache }}}}\" = \"$RUNNER_TOOL_CACHE\" ] && echo expression-matches'\n"
    )
}

fn tool_cache_line(report: &RunReportPlus) -> String {
    let lines = log_lines(report);
    // The expression must land on the same path the variable carries, in
    // every branch of the resolution below.
    assert!(
        lines.iter().any(|l| l == "expression-matches"),
        "`runner.tool_cache` equals `RUNNER_TOOL_CACHE`: {lines:?}"
    );
    lines
        .iter()
        .find(|l| l.starts_with("tool-cache="))
        .expect("the probe printed its tool cache")
        .clone()
}

/// A host job with a registered persistent cache uses it; the run's own
/// workspace stays out of the picture.
#[tokio::test(flavor = "multi_thread")]
async fn host_jobs_use_the_persistent_tool_cache() {
    let store = env::temp_dir()
        .join("petri-toolcache-e2e")
        .join(format!("store-{}", process::id()));
    fs::create_dir_all(&store).expect("create the store");
    let graph = lower_ok(&workflow(None));
    let report = run_host_with(graph, "toolcache-host", |rt| {
        rt.capability(ToolCacheCap(PathBuf::from(&store)))
    })
    .await;
    assert_eq!(
        tool_cache_line(&report),
        format!("tool-cache={}", store.display())
    );
    let _ = fs::remove_dir_all(&store);
}

/// A runner image that ships a populated tool cache keeps it: the image's env
/// names `/opt/hostedtoolcache`, and the resolution never overrides an
/// environment that already answered — the ambient value wins for the
/// exported variable and the expression alike.
#[tokio::test(flavor = "multi_thread")]
async fn containerized_jobs_keep_the_image_tool_cache() {
    if !testkit::docker_ready().await {
        return;
    }
    let graph = lower_ok(&workflow(Some(RUNNER_IMAGE_2404)));
    let report = run_host(graph, "toolcache-boxed").await;
    assert_eq!(tool_cache_line(&report), "tool-cache=/opt/hostedtoolcache");
}

/// With no persistent store and no image answer, the workspace directory
/// stands in — the pre-store behavior, per run.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_store_the_workspace_stands_in() {
    let graph = lower_ok(&workflow(None));
    let report = run_host(graph, "toolcache-bare").await;
    let line = tool_cache_line(&report);
    assert!(
        line.ends_with("/.ci/toolcache"),
        "the workspace fallback: {line}"
    );
}
