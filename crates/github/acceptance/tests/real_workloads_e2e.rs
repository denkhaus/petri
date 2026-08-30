//! Real workloads, tiny and deterministic: the corpus sweep stubs every
//! `run:` script to `true`, so nothing there proves that an *actual build
//! script* runs inside the runner containers — that the image's compilers
//! compile, that installed packages import, that npm's output executes. These
//! probes are the smallest real work of each kind, pinned to exact versions,
//! against the pinned runner image. They are mechanism evidence, not a
//! benchmark: a few seconds each, in CI on every run with Docker.
//!
//! The workflows live as plain YAML beside this file (`real_workloads/`), so
//! they read and edit as workflows; `__IMAGE__` is the one substitution.
//!
//! Deliberately container-only: the target is the runner image's toolchain
//! surface, not the developer's host. Two kinds of probe: image-native tools
//! (gcc, python, node) run directly, and the toolchains slim deliberately
//! does not ship (Go, Ruby, Rust) arrive the way real workflows get them —
//! through their sha-pinned setup actions into the tool cache, then a real
//! build — so those probes cover the whole action → tool cache → PATH →
//! compile chain.

mod support;

use std::sync::Arc;

use acceptance::runs::RUNNER_IMAGE_2404;
use github_actions::{ActionSourceCap, ActionTreeSource};
use support::*;

/// Run one `real_workloads/*.yaml` file against the pinned runner image and
/// assert the marker line its real build prints.
async fn assert_workload(yaml: &str, label: &str, expected: &str) {
    if !testkit::is_docker_ready().await {
        return;
    }
    let text = yaml.replace("__IMAGE__", RUNNER_IMAGE_2404);
    let graph = lower_ok(&text);
    let report = run_host(graph, label).await;
    assert_success(&report);
    let lines = log_lines(&report);
    assert!(
        lines.iter().any(|l| l == expected),
        "no `{expected}` in {lines:#?}"
    );
}

/// [`assert_workload`] for a workflow whose toolchain arrives through a real
/// `uses:` action — lowered and run against the corpus action cache (fetched
/// on machines that don't have it).
async fn assert_action_workload(yaml: &str, label: &str, expected: &str) {
    if !testkit::is_docker_ready().await {
        return;
    }
    let text = yaml.replace("__IMAGE__", RUNNER_IMAGE_2404);
    let source = corpus_action_source();
    let graph = lower_with_actions(&text, &source);
    let trees: Arc<dyn ActionTreeSource> = source;
    let report = run_host_with(graph, label, |rt| rt.capability(ActionSourceCap(trees))).await;
    assert_success(&report);
    let lines = log_lines(&report);
    assert!(
        lines.iter().any(|l| l == expected),
        "no `{expected}` in {lines:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_c_program_compiles_and_runs() {
    assert_workload(
        include_str!("real_workloads/c.yaml"),
        "real-c",
        "c-built-and-ran",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_wheel_installs_and_imports() {
    assert_workload(
        include_str!("real_workloads/python.yaml"),
        "real-python",
        "python-installed-and-imported 1.17.0",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_npm_package_installs_and_runs() {
    assert_workload(
        include_str!("real_workloads/node.yaml"),
        "real-node",
        "node-installed-and-ran 007",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_go_installs_a_toolchain_that_builds() {
    assert_action_workload(
        include_str!("real_workloads/go.yaml"),
        "real-go",
        "go-built-and-ran 1+1=2",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_ruby_installs_a_ruby_whose_gems_run() {
    assert_action_workload(
        include_str!("real_workloads/ruby.yaml"),
        "real-ruby",
        "ruby-installed-and-ran 3.3.6",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_toolchain_installs_a_rustc_that_compiles() {
    assert_action_workload(
        include_str!("real_workloads/rust.yaml"),
        "real-rust",
        "rust-built-and-ran 42",
    )
    .await;
}
