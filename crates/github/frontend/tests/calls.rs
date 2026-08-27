//! Reusable workflows and typed inputs: what the lowering produces.
//! The behavioural half runs in `crates/github/acceptance/tests/gha_e2e.rs`.

mod support;

use frontend::{NoFiles, Severity};
use frontend_gha::action::MapActionSource;
use frontend_gha::load_with;
use serde_json::json;
use support::*;

const CALLEE: &str = r#"
on:
  workflow_call:
    inputs:
      version:
        type: string
        required: true
      fast:
        type: boolean
        default: true
    outputs:
      artifact:
        value: ${{ jobs.build.outputs.artifact }}
    secrets:
      token:
        required: false
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      artifact: ${{ steps.pack.outputs.name }}
    steps:
      - id: pack
        run: echo "name=app-${{ inputs.version }}" >> "$GITHUB_OUTPUT"
  check:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo "checking ${{ needs.build.outputs.artifact }} fast=${{ inputs.fast }}"
        env:
          TOKEN: ${{ secrets.token }}
"#;

const CALLER: &str = r#"
on: push
jobs:
  release:
    uses: ./.github/workflows/build.yml
    with:
      version: "1.2"
    secrets: inherit
  announce:
    needs: release
    runs-on: ubuntu-latest
    steps:
      - run: echo "released ${{ needs.release.outputs.artifact }}"
"#;

#[test]
fn a_local_call_inlines_the_callee_under_the_call_job() {
    let files = files(&[(".github/workflows/build.yml", CALLEE)]);
    let graph = lower_ok_with(CALLER, &files);
    let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
    // The call bracket, the inlined jobs under the call's prefix, and the
    // caller's own job all coexist.
    for wanted in [
        "release/start",
        "release/exit",
        "release/done",
        "release/build/start",
        "release/build/pack",
        "release/build/done",
        "release/check/start",
        "release/check/step-1",
        "release/check/done",
        "announce/start",
        "announce/step-1",
    ] {
        assert!(names.contains(&wanted), "missing {wanted} in {names:?}");
    }
    // The call is preserved on its start.
    let start = graph.nodes.iter().find(|n| n.name == "release/start").unwrap();
    assert_eq!(
        start.meta["call"]["uses"],
        json!("./.github/workflows/build.yml")
    );
    // Each inlined job keeps its own scope and placement.
    let build = graph
        .nodes
        .iter()
        .find(|n| n.name == "release/build/start")
        .unwrap();
    let scope = graph.scope(build.scope).unwrap();
    assert_eq!(scope.runtime.requirements, ["ubuntu-latest"]);
}

#[test]
fn dispatch_inputs_lower_from_run_parameters() {
    let text = r#"
on:
  workflow_dispatch:
    inputs:
      environment:
        type: choice
        options: [staging, production]
        default: staging
      dry-run:
        type: boolean
        default: true
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - run: echo "to=${{ inputs.environment }} dry=${{ inputs.dry-run }}"
"#;
    let graph = lower_ok(text);
    assert!(!graph.nodes.is_empty());
    let diags = diagnostics(text);
    assert!(
        !diags.iter().any(|d| d.severity == Severity::Error),
        "{diags:?}"
    );
}

/// A reusable file loaded on its own binds `inputs` from run parameters, so it
/// can be run directly — with typed defaults filling the gaps.
#[test]
fn a_reusable_file_lowers_standalone() {
    let files = files(&[]);
    let lowered = frontend_gha::load(".github/workflows/build.yml", CALLEE, &files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    assert!(lowered.graph.is_some());
}

#[test]
fn call_contract_violations_are_specific_errors() {
    let files = files(&[(".github/workflows/build.yml", CALLEE)]);
    for (bad, code, wants) in [
        // Required input missing.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/build.yml\n".to_string(),
            "gha.missing_input",
            "version",
        ),
        // An input the callee never declared.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/build.yml\n    with:\n      version: \"1\"\n      nope: x\n".to_string(),
            "gha.unknown_input",
            "nope",
        ),
        // A boolean input with a non-boolean value.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/build.yml\n    with:\n      version: \"1\"\n      fast: maybe\n".to_string(),
            "gha.input_type",
            "boolean",
        ),
        // A secret the callee never declared.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/build.yml\n    with:\n      version: \"1\"\n    secrets:\n      other: ${{ secrets.X }}\n".to_string(),
            "gha.bad_call",
            "not declared",
        ),
        // A call job cannot carry job keys.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/build.yml\n    runs-on: ubuntu-latest\n    with:\n      version: \"1\"\n".to_string(),
            "gha.bad_call",
            "runs-on",
        ),
        // A callee that is not reusable.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/plain.yml\n".to_string(),
            "gha.not_reusable",
            "workflow_call",
        ),
        // A local file that does not exist.
        (
            "on: push\njobs:\n  j:\n    uses: ./.github/workflows/missing.yml\n".to_string(),
            "gha.bad_call",
            "no workflow",
        ),
    ] {
        let files = files_with_plain(&files);
        let diags = diagnostics_with(&bad, &files);
        let d = diags
            .iter()
            .find(|d| d.code == code)
            .unwrap_or_else(|| panic!("{bad}: wanted {code}, got {diags:?}"));
        assert!(
            d.message.contains(wants) || d.hint.as_deref().unwrap_or("").contains(wants),
            "{bad}: {d:?}"
        );
    }
}

fn files_with_plain(base: &frontend::MapFiles) -> frontend::MapFiles {
    let mut map = base.0.clone();
    map.insert(
        ".github/workflows/plain.yml".into(),
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n"
            .into(),
    );
    frontend::MapFiles(map)
}

#[test]
fn call_cycles_and_depth_are_diagnostics() {
    let a = "on: [push, workflow_call]\njobs:\n  j:\n    uses: ./.github/workflows/b.yml\n";
    let b = "on: [workflow_call]\njobs:\n  j:\n    uses: ./.github/workflows/a.yml\n";
    let files = files(&[
        (".github/workflows/a.yml", a),
        (".github/workflows/b.yml", b),
    ]);
    let diags = diagnostics_with(a, &files);
    assert!(
        diags.iter().any(|d| d.code == "gha.workflow_cycle"),
        "{diags:?}"
    );
}

/// A matrix on the call expands the whole inlined workflow per leg; a matrix
/// inside a matrix call cannot nest, and says so.
#[test]
fn a_matrix_call_expands_and_nesting_is_rejected() {
    let callee = r#"
on:
  workflow_call:
    inputs:
      version: { type: string, required: true }
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo "v=${{ inputs.version }}"
"#;
    let caller = r#"
on: push
jobs:
  fan:
    strategy:
      matrix:
        v: ["1", "2"]
    uses: ./.github/workflows/build.yml
    with:
      version: ${{ matrix.v }}
"#;
    let call_files = files(&[(".github/workflows/build.yml", callee)]);
    let graph = lower_ok_with(caller, &call_files);
    let start = graph.nodes.iter().find(|n| n.name == "fan/start").unwrap();
    assert!(start.expand.is_some(), "the call start is the expansion head");

    let nested_callee = r#"
on:
  workflow_call: {}
jobs:
  build:
    strategy:
      matrix:
        os: [ubuntu-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo
"#;
    let caller = r#"
on: push
jobs:
  fan:
    strategy:
      matrix:
        v: ["1"]
    uses: ./.github/workflows/nested.yml
"#;
    let nested_files = files(&[(".github/workflows/nested.yml", nested_callee)]);
    let diags = diagnostics_with(caller, &nested_files);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "unsupported.workflow_call.matrix"),
        "{diags:?}"
    );
}

/// A remote call resolves through the action source: pinned, fetched, inlined —
/// and the pin lands on the call's start meta.
#[test]
fn a_remote_call_resolves_through_the_action_source() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let callee = "on:\n  workflow_call: {}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo remote\n";
    let source = MapActionSource::new().with(
        &format!("octo/shared/.github/workflows/build.yml@{sha}"),
        sha,
        callee,
    );
    let text = format!(
        "on: push\njobs:\n  j:\n    uses: octo/shared/.github/workflows/build.yml@{sha}\n"
    );
    let lowered = load_with(".github/workflows/test.yml", &text, &NoFiles, Some(&source));
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = lowered.graph.expect("a graph");
    let start = graph.nodes.iter().find(|n| n.name == "j/start").unwrap();
    assert_eq!(start.meta["call"]["sha"], json!(sha));
    assert!(graph.nodes.iter().any(|n| n.name == "j/build/step-1"));
}
