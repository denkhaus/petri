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
//! surface, not the developer's host. And deliberately image-native tools
//! (gcc, python, node) — Go/Ruby/Rust arrive via setup-* actions, whose
//! staging the toolcache and action batteries already cover.

mod support;

use runtime::ir::RunStatus;
use support::*;

/// Run one `real_workloads/*.yaml` file against the pinned runner image and
/// assert the marker line its real build prints.
async fn assert_workload(yaml: &str, label: &str, expected: &str) {
    if !testkit::docker_ready().await {
        return;
    }
    let text = yaml.replace("__IMAGE__", acceptance::runs::RUNNER_IMAGE_2404);
    let graph = lower_ok(&text);
    let report = run_host(graph, label).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
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
