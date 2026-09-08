//! Read a finished run back through the public command, `petri inspect
//! --run-dir <dir> --json`. The harness never opens the run directory's
//! logs itself: what the command prints is what a user or an embedding host
//! can see.

use std::path::Path;
use std::process::{Command, Stdio};
use std::{env, fs};

use serde_json::Value;

/// The `petri inspect --json` document for `run_dir`. The command runs with
/// an empty environment plus `PATH`: inspection needs no provider, plugin, or
/// credential.
///
/// # Panics
///
/// Panics when the command fails or prints something other than JSON: the
/// run directory is not a finished run, which the caller asserts first.
pub(crate) fn inspect(run_dir: &Path) -> Value {
    let path = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let output = Command::new(env!("CARGO_BIN_EXE_petri"))
        .env_clear()
        .env("PATH", path)
        .args(["inspect", "--json", "--run-dir"])
        .arg(run_dir)
        .stdin(Stdio::null())
        .output()
        .expect("petri inspect runs");
    assert!(
        output.status.success(),
        "petri inspect --run-dir {} failed:\n{}",
        run_dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "petri inspect printed something other than JSON: {error}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// A value as a user resolves it: a `blob://sha256/<hex>` reference (Petri's
/// output store, `<run_dir>/blobs/<hex>`, `#json` for a structured value) is
/// read back; anything else is returned as it is. The blob directory is the
/// one part of the run directory a reader opens by hand, because the inspect
/// document shows the reference and names where it resolves.
pub(crate) fn resolve_reference(value: Value, run_dir: &Path) -> Value {
    let Value::String(text) = &value else {
        return value;
    };
    let (body, json) = match text.strip_suffix("#json") {
        Some(body) => (body, true),
        None => (text.as_str(), false),
    };
    let Some(hex) = body.strip_prefix("blob://sha256/") else {
        return value;
    };
    let path = run_dir.join("blobs").join(hex);
    let bytes = fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "the reference {text} resolves under {}: {error}",
            path.display()
        )
    });
    if json {
        serde_json::from_slice(&bytes).expect("a structured blob is JSON")
    } else {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// The root invocation's final context from an inspect document.
pub(crate) fn root_context(document: &Value) -> &Value {
    &document["invocations"][0]["result"]["context"]
}

/// The root execution's node-instance records from an inspect document.
pub(crate) fn root_nodes(document: &Value) -> &Value {
    let execution = document["root"]["final_execution"]
        .as_u64()
        .expect("the root invocation finished");
    let executions = document["executions"]
        .as_array()
        .expect("executions is a list");
    let record = executions
        .iter()
        .find(|e| e["execution"] == execution)
        .expect("the final execution is listed");
    &record["engine"]["context"]["nodes"]
}
