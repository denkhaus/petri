//! Real workloads, tiny and deterministic: the corpus sweep stubs every
//! `run:` script to `true`, so nothing there proves that an *actual build
//! script* runs inside the runner containers — that the image's compilers
//! compile, that installed packages import, that npm's output executes. These
//! probes are the smallest real work of each kind, pinned to exact versions,
//! against the pinned runner image. They are mechanism evidence, not a
//! benchmark: a few seconds each, in CI on every run with Docker.
//!
//! Deliberately container-only: the target is the runner image's toolchain
//! surface, not the developer's host. And deliberately image-native tools
//! (gcc, python, node) — Go/Ruby/Rust arrive via setup-* actions, whose
//! staging the toolcache and action batteries already cover.

mod support;

use runtime::ir::RunStatus;
use support::*;

/// Three real builds, one job each: C compiled and executed, a pinned wheel
/// installed into a venv and imported, a pinned npm package installed and
/// required. Every step is a real `run:` script in the runner container.
const REAL_WORKLOADS: &str = r#"
on: push
jobs:
  c:
    runs-on: ubuntu-latest
    container: __IMAGE__
    steps:
      - run: |
          cat > hello.c <<'EOF'
          #include <stdio.h>
          int main(void) { printf("c-built-and-ran\n"); return 0; }
          EOF
          gcc -o hello hello.c
          ./hello
  python:
    runs-on: ubuntu-latest
    container: __IMAGE__
    steps:
      - run: |
          python3 -m venv v
          ./v/bin/pip --quiet install six==1.17.0
          ./v/bin/python -c "import six; print('python-installed-and-imported', six.__version__)"
  node:
    runs-on: ubuntu-latest
    container: __IMAGE__
    steps:
      - run: |
          npm init -y >/dev/null
          npm install --no-audit --no-fund left-pad@1.3.0 >/dev/null
          node -e "console.log('node-installed-and-ran', require('left-pad')('7', 3, '0'))"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn real_build_scripts_run_in_the_runner_image() {
    if !testkit::docker_ready().await {
        return;
    }
    let text = REAL_WORKLOADS.replace("__IMAGE__", acceptance::runs::RUNNER_IMAGE_2404);
    let graph = lower_ok(&text);
    let report = run_host(graph, "real-workloads").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    for expected in [
        "c-built-and-ran",
        "python-installed-and-imported 1.17.0",
        "node-installed-and-ran 007",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "no `{expected}` in {lines:#?}"
        );
    }
}
