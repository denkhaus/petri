//! The shipped binary runs a Fabro workflow with no Fabro executable
//! anywhere on `PATH`, and never carries a Fabro launch command: production
//! Petri does not require or launch Fabro.

use std::path::Path;
use std::process::Command;
use std::{env, fs};

use testkit::RunDir;

/// A `PATH` with an empty directory in front and only the system binaries
/// behind it. `None` when a `fabro` lives in one of those system directories,
/// which this test cannot sanitize.
fn sanitized_path(dir: &RunDir) -> Option<String> {
    let empty = dir.path().join("empty-bin");
    fs::create_dir_all(&empty).expect("the empty bin dir");
    let system = ["/usr/bin", "/bin"];
    if system.iter().any(|d| Path::new(d).join("fabro").exists()) {
        return None;
    }
    Some(format!("{}:{}", empty.display(), system.join(":")))
}

/// `petri`, with the environment cleared except for what a run needs: the
/// sanitized `PATH`, `HOME` and `TMPDIR`, and the sandbox plugin settings
/// the caller exported (`PETRI_SANDBOX_*`).
fn petri(path: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_petri"));
    command.env_clear();
    command.env("PATH", path);
    for key in ["HOME", "TMPDIR"] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in env::vars_os() {
        if key.to_string_lossy().starts_with("PETRI_SANDBOX_") {
            command.env(key, value);
        }
    }
    command
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr"
)]
fn petri_runs_a_fabro_workflow_with_no_fabro_on_path() {
    let dir = RunDir::new("standalone-no-fabro");
    let Some(path) = sanitized_path(&dir) else {
        eprintln!("skipping: a fabro executable lives in a system bin directory");
        return;
    };
    let workflow = dir.path().join("plain.fabro");
    fs::write(
        &workflow,
        r#"digraph Plain {
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
    // `fabro` really is unreachable under this PATH.
    let probe = Command::new("fabro")
        .env_clear()
        .env("PATH", &path)
        .arg("--version")
        .output();
    assert!(probe.is_err(), "fabro resolved under the sanitized PATH");

    let checked = petri(&path)
        .arg("check")
        .arg(&workflow)
        .output()
        .expect("petri runs");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    let run_dir = dir.path().join("run");
    let ran = petri(&path)
        .args(["run", "--quiet", "--dry-run", "--run-dir"])
        .arg(&run_dir)
        .arg(&workflow)
        .output()
        .expect("petri runs");
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(ran.status.success(), "{stderr}");
    assert!(stderr.contains("run: success"), "{stderr}");
    assert!(stderr.contains("success build"), "{stderr}");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn petri_binary_does_not_reference_a_fabro_executable() {
    let bytes = fs::read(env!("CARGO_BIN_EXE_petri")).expect("the petri binary reads");
    for needle in [
        b"fabro server start".as_slice(),
        b"fabro run --".as_slice(),
        b"/bin/fabro".as_slice(),
    ] {
        assert!(
            !contains(&bytes, needle),
            "the petri binary carries a Fabro launch string: {}",
            String::from_utf8_lossy(needle)
        );
    }
}
