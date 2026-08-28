//! Environment names outside the shell-identifier alphabet reach the step.
//!
//! GitHub's own contract requires it: the toolkit reads hyphenated
//! `INPUT_INCLUDE-HIDDEN-FILES`-style names, and workflows write hyphenated
//! `env:` keys. A dash `/bin/sh` *filters such names out* when it spawns
//! children, so every wrapper between the executor and the step prefers bash —
//! this is the regression test that keeps it that way on a Linux host, where
//! `/bin/sh` really is dash. (The containerized counterpart rides the
//! artifacts battery: `upload-artifact` hard-requires its hyphenated input.)

mod support;

use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn hyphenated_env_names_reach_the_step() {
    let graph = lower_ok(
        "on: push\n\
         jobs:\n\
         \x20 probe:\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   steps:\n\
         \x20     - run: printenv not-a-shell-name\n\
         \x20       env:\n\
         \x20         not-a-shell-name: survived\n",
    );
    let report = run_host(graph, "env-names").await;
    assert_eq!(report.status, runtime::ir::RunStatus::Success);
    assert!(
        log_lines(&report).iter().any(|l| l == "survived"),
        "the hyphenated name reached the step: {:?}",
        log_lines(&report)
    );
}
