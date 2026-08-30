//! The local-checkout substitution: which `actions/checkout` calls become the
//! `github/checkout` step, and which fall through to the real action. The
//! tests run sourceless, so a fall-through shows as the reference's
//! `action.remote` rejection — the sharpest possible assertion that the real
//! action would have been fetched.

mod support;

use frontend_gha::{CHECKOUT_KIND, RunnerMap, load_configured};
use support::*;

fn checkout_workflow(with: &str) -> String {
    format!(
        "on: push\n\
         jobs:\n\
         \x20 build:\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   steps:\n\
         \x20     - uses: actions/checkout@v4\n\
         {with}\
         \x20     - run: ls\n"
    )
}

fn checkout_kinds(text: &str, files: &dyn frontend::FileSource) -> Vec<String> {
    let lowered = frontend_gha::load(".github/workflows/test.yml", text, files);
    let graph = lowered.graph.expect("the workflow lowers");
    graph
        .nodes
        .iter()
        .filter(|n| n.name.contains("step"))
        .map(|n| n.step.kind.to_string())
        .collect()
}

#[test]
fn a_default_checkout_substitutes() {
    let kinds = checkout_kinds(&checkout_workflow(""), &frontend::NoFiles);
    assert_eq!(kinds, vec![
        CHECKOUT_KIND.to_string(),
        "github/run".to_string()
    ]);
}

#[test]
fn path_is_honored_and_clone_policy_inputs_are_ignored() {
    let with = "\x20       with:\n\
                \x20         path: sub/dir\n\
                \x20         fetch-depth: 0\n\
                \x20         persist-credentials: false\n";
    let text = checkout_workflow(with);
    let lowered = frontend_gha::load(".github/workflows/test.yml", &text, &frontend::NoFiles);
    let graph = lowered.graph.expect("lowers");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.step.kind.as_ref() == CHECKOUT_KIND)
        .expect("the substituted node");
    assert_eq!(node.step.config["path"], serde_json::json!("sub/dir"));
    assert!(
        node.step.config.get("source").is_some(),
        "the source rides a run-parameter expression"
    );
}

#[test]
fn the_substituted_config_carries_run_identity() {
    let lowered = frontend_gha::load(
        ".github/workflows/test.yml",
        &checkout_workflow(""),
        &frontend::NoFiles,
    );
    let graph = lowered.graph.expect("lowers");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.step.kind.as_ref() == CHECKOUT_KIND)
        .expect("the substituted node");
    // The step shapes the snapshot's git state from the run's identity, each
    // field riding a firing-time expression like `source`.
    for key in ["ref", "repository", "server_url"] {
        assert!(
            node.step.config[key].get("$expr").is_some(),
            "`{key}` rides a run-parameter expression: {:?}",
            node.step.config
        );
    }
}

#[test]
fn a_spelled_out_own_repository_and_ref_still_substitute() {
    let config = "[remote \"origin\"]\n\turl = git@github.com:octo/widget.git\n";
    let files = files(&[(".git/config", config)]);
    let with = "\x20       with:\n\
                \x20         repository: octo/widget\n\
                \x20         ref: main\n";
    let kinds = checkout_kinds(&checkout_workflow(with), &files);
    assert_eq!(kinds[0], CHECKOUT_KIND);
}

#[test]
fn everything_else_falls_through_to_the_real_action() {
    for with in [
        // Another repository.
        "\x20       with:\n\x20         repository: octo/other\n",
        // A non-default ref.
        "\x20       with:\n\x20         ref: release-1.0\n",
        // An expression value.
        "\x20       with:\n\x20         ref: ${{ github.head_ref }}\n",
        // Submodules, a token: real-action business.
        "\x20       with:\n\x20         submodules: recursive\n",
        "\x20       with:\n\x20         token: ${{ secrets.PAT }}\n",
    ] {
        let diags = diagnostics(&checkout_workflow(with));
        assert!(
            diags.iter().any(|d| d.code == "unsupported.action.remote"),
            "fell through to the real (sourceless) action: {with:?} -> {diags:?}"
        );
    }
}

#[test]
fn the_off_switch_keeps_the_real_action() {
    let lowered = load_configured(
        ".github/workflows/test.yml",
        &checkout_workflow(""),
        &frontend::NoFiles,
        None,
        &RunnerMap::builtin(),
        false,
    );
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "unsupported.action.remote"),
        "with substitution off, checkout is the real action"
    );
}
