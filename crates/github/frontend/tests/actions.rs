//! `uses: owner/repo@ref` lowering: a JavaScript action becomes `github/action`
//! nodes — main where the step is, `pre` before every step, `post` after the last
//! in reverse — pinned to the commit the source resolved, with inputs and
//! `github.token` lowered where the step is.

use frontend::NoFiles;
use frontend_gha::action::MapActionSource;
use frontend_gha::{ACTION_KIND, RUN_KIND, STATE_OUTPUT_KEY, load, load_with};
use serde_json::json;

const CHECKOUT: &str = r#"
name: Checkout
inputs:
  repository:
    default: ${{ github.repository }}
  token:
    default: ${{ github.token }}
  fetch-depth:
    default: 1
runs:
  using: node20
  main: dist/index.js
  post: dist/cleanup.js
"#;

const WITH_PRE: &str = r#"
name: Pre and post
runs:
  using: node24
  main: main.js
  pre: pre.js
  pre-if: runner.os == 'Linux'
  post: post.js
  post-if: success()
"#;

fn lower(text: &str, source: &MapActionSource) -> frontend::Lowered {
    let lowered = load_with(".github/workflows/ci.yml", text, &NoFiles, Some(source));
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

/// The step chain in creation order — every node but a job's `start` and `done`,
/// which the shell pass creates before any step.
fn chain(graph: &ir::Graph) -> Vec<&str> {
    graph
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .filter(|n| !n.ends_with("/start") && !n.ends_with("/done"))
        .collect()
}

#[test]
fn a_node_action_lowers_to_a_main_node_and_a_trailing_post_node() {
    let source = MapActionSource::new().with("actions/checkout@v4", "0123abcd0123abcd", CHECKOUT);
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - run: echo hi
"#;
    let graph = lower(text, &source).graph.expect("lowers");
    assert_eq!(
        chain(&graph),
        vec!["build/step-1", "build/step-2", "build/step-1/post"]
    );

    let main = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-1")
        .unwrap();
    assert_eq!(main.step.kind.to_string(), ACTION_KIND);
    let config = &main.step.config;
    assert_eq!(config["phase"], "main");
    assert_eq!(config["entry"], "dist/index.js");
    assert_eq!(config["runtime"], "node20");
    assert_eq!(config["action"]["sha"], "0123abcd0123abcd");
    assert_eq!(config["action"]["reference"]["owner"], "actions");
    assert_eq!(config["action"]["reference"]["ref"], "v4");
    // The caller's value wins over the default; a number is passed as text.
    assert_eq!(config["inputs"]["fetch-depth"], "0");
    // `github.token` is a secret reference, never a value.
    assert_eq!(
        config["inputs"]["token"],
        json!({ "$secret": "GITHUB_TOKEN" })
    );
    // `github.repository` is an expression over the run parameters.
    assert!(config["inputs"]["repository"].get("$expr").is_some());
    assert!(config.get("state").is_none(), "main has no earlier phase");

    let post = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-1/post")
        .unwrap();
    assert_eq!(post.step.kind.to_string(), ACTION_KIND);
    assert_eq!(post.step.config["phase"], "post");
    assert_eq!(post.step.config["entry"], "dist/cleanup.js");
    assert!(
        post.step.config["state"].get("$expr").is_some(),
        "post reads main's saved state"
    );
    assert!(post.run_on_cancel, "a default `post-if` is `always()`");

    let run = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-2")
        .unwrap();
    assert_eq!(run.step.kind.to_string(), RUN_KIND);
    assert!(run.step.config.get("output_env_aliases").is_none());
    assert!(run.step.config["event"].get("$expr").is_some());
    let _ = STATE_OUTPUT_KEY;
}

#[test]
fn pre_nodes_come_first_and_post_nodes_last_in_reverse() {
    let source = MapActionSource::new()
        .with("acme/prepost@v1", "aaaa", WITH_PRE)
        .with("actions/checkout@v4", "bbbb", CHECKOUT);
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo first
      - id: a
        uses: acme/prepost@v1
      - id: b
        uses: actions/checkout@v4
"#;
    let graph = lower(text, &source).graph.expect("lowers");
    assert_eq!(
        chain(&graph),
        vec!["j/a/pre", "j/step-1", "j/a", "j/b", "j/b/post", "j/a/post"]
    );
    let a = graph.nodes.iter().find(|n| n.name == "j/a").unwrap();
    assert!(
        a.step.config["state"].get("$expr").is_some(),
        "main reads pre's saved state"
    );
    let a_post = graph.nodes.iter().find(|n| n.name == "j/a/post").unwrap();
    assert!(
        !a_post.run_on_cancel,
        "`post-if: success()` does not run after a cancel"
    );
}

#[test]
fn a_required_input_without_a_value_is_an_error() {
    let source = MapActionSource::new().with(
        "acme/needs@v1",
        "cccc",
        "inputs:\n  who:\n    required: true\nruns:\n  using: node20\n  main: index.js\n",
    );
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/needs@v1
"#;
    let lowered = lower(text, &source);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .errors()
            .any(|d| d.to_string().contains("requires input `who`")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn without_a_source_remote_actions_stay_unsupported() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
"#;
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.unsupported_feature() == Some("action.remote")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn an_unresolvable_reference_is_an_error_not_an_unsupported_feature() {
    let source = MapActionSource::new();
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: nobody/nothing@v9
"#;
    let lowered = lower(text, &source);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .errors()
            .any(|d| d.to_string().contains("nobody/nothing@v9")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn secrets_lower_in_step_config_and_nowhere_else() {
    // Whole value: a `$secret` reference the process step resolves.
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
        env:
          T: ${{ github.token }}
"#;
    let graph = load(".github/workflows/ci.yml", text, &NoFiles)
        .graph
        .expect("lowers");
    let run = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    assert_eq!(
        run.step.config["env"]["T"],
        json!({ "$secret": "GITHUB_TOKEN" })
    );

    // Inside a larger string or expression in step config: lowers, as an
    // expression the step resolves the sentinel out of at spawn.
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo "token is ${{ github.token }}"
        env:
          AUTH: ${{ github.server_url == 'https://github.com' && github.token || '' }}
"#;
    let graph = load(".github/workflows/ci.yml", text, &NoFiles)
        .graph
        .expect("lowers");
    let run = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    assert!(run.step.config["run"].get("$expr").is_some());
    assert!(run.step.config["env"]["AUTH"].get("$expr").is_some());

    // Where the engine would evaluate it — an `if:` — a secret is rejected.
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
        if: secrets.DEPLOY_KEY != ''
"#;
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.unsupported_feature() == Some("secrets.expression")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}
