//! The shipped binary runs a Fabro workflow and answers its human gate with
//! `--auto-approve`: the in-process answer surface, end to end.

use std::path::PathBuf;
use std::process::{self, Command};
use std::{env, fs};

fn temp(label: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("petri-fabro-cli-{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn petri_run_auto_approve_answers_a_human_gate() {
    let dir = temp("auto-approve");
    let workflow = dir.join("gate.fabro");
    fs::write(
        &workflow,
        r#"digraph Gate {
            start [shape=Mdiamond]
            exit [shape=Msquare]
            gate [shape=hexagon, label="Ship it?"]
            yes [shape=parallelogram, script="echo shipped"]
            no [shape=parallelogram, script="echo held"]
            start -> gate
            gate -> yes [label="[Y] Yes"]
            gate -> no [label="[N] No"]
            yes -> exit
            no -> exit
        }"#,
    )
    .expect("write the workflow");
    let run_dir = dir.join("run");
    let output = Command::new(env!("CARGO_BIN_EXE_petri"))
        .args(["run", "--quiet", "--auto-approve", "--run-dir"])
        .arg(&run_dir)
        .arg(&workflow)
        .output()
        .expect("petri runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("success yes"),
        "the first choice was taken:\n{stderr}"
    );
    assert!(!stderr.contains("success no"), "{stderr}");
    assert!(stderr.contains("run: success"), "{stderr}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn petri_check_lowers_a_fabro_file_and_rejects_the_deprecated_spelling() {
    let dir = temp("check");
    let good = dir.join("good.fabro");
    fs::write(
        &good,
        "digraph G { start [shape=Mdiamond] exit [shape=Msquare] a [prompt=\"x\"] start -> a -> exit }",
    )
    .expect("write");
    let bad = dir.join("bad.fabro");
    fs::write(
        &bad,
        "digraph G { start [shape=Mdiamond] exit [shape=Msquare] a [prompt=\"x\", on_failure=\"succeed\"] start -> a -> exit }",
    )
    .expect("write");
    let ok = Command::new(env!("CARGO_BIN_EXE_petri"))
        .args(["check", "--print-graph"])
        .arg(&good)
        .output()
        .expect("petri runs");
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let printed = String::from_utf8_lossy(&ok.stdout);
    assert!(printed.contains("step=fabro/agent"), "{printed}");
    let rejected = Command::new(env!("CARGO_BIN_EXE_petri"))
        .args(["check"])
        .arg(&bad)
        .output()
        .expect("petri runs");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("unsupported.on_failure.succeed"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn petri_run_dry_run_simulates_the_fabro_stages() {
    let dir = temp("dry-run");
    let workflow = dir.join("agents.fabro");
    fs::write(
        &workflow,
        r#"digraph Agents {
            start [shape=Mdiamond]
            exit [shape=Msquare]
            plan [prompt="Plan"]
            gate [shape=hexagon, label="Go?"]
            build [prompt="Build"]
            start -> plan -> gate
            gate -> build [label="[G] Go"]
            gate -> exit [label="[S] Stop"]
            build -> exit
        }"#,
    )
    .expect("write the workflow");
    let run_dir = dir.join("run");
    let output = Command::new(env!("CARGO_BIN_EXE_petri"))
        .args(["run", "--quiet", "--dry-run", "--run-dir"])
        .arg(&run_dir)
        .arg(&workflow)
        .output()
        .expect("petri runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    for stage in ["plan", "gate", "build", "exit"] {
        assert!(
            stderr.contains(&format!("success {stage}")),
            "{stage}:\n{stderr}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}
