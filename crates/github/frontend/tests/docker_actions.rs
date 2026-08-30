//! Docker container action lowering: `uses: docker://…` and `runs.using:
//! docker` become `github/docker_action` nodes — main where the step is,
//! `pre-entrypoint` before every step, `post-entrypoint` after the last — with
//! the image, entrypoints and args carried in the config and the action's
//! inputs bound for the manifest's own expressions.

use frontend::{MapFiles, NoFiles};
use frontend_gha::action::MapActionSource;
use frontend_gha::{DOCKER_ACTION_KIND, load, load_with};
use serde_json::json;

const TEST_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

const DOCKER_MANIFEST: &str = r#"
name: Publish
inputs:
  message:
    default: hello
  token:
    required: true
runs:
  using: docker
  image: Dockerfile
  entrypoint: /entry.sh
  pre-entrypoint: /pre.sh
  post-entrypoint: /post.sh
  post-if: always()
  args:
    - ${{ inputs.message }}
    - literal
  env:
    GREETING: ${{ inputs.message }}
"#;

fn lower(text: &str, source: &MapActionSource) -> frontend::Lowered {
    let lowered = load_with(".github/workflows/ci.yml", text, &NoFiles, Some(source));
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

fn node<'g>(graph: &'g ir::Graph, name: &str) -> &'g ir::Node {
    graph
        .nodes
        .iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("no node named {name}"))
}

#[test]
fn a_docker_url_step_lowers_with_args_and_entrypoint() {
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: docker://alpine:3.21
        with:
          entrypoint: /bin/echo
          args: hello ${{ github.ref_name }}
          who: world
"#;
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = lowered.graph.expect("lowers");
    let step = node(&graph, "build/step-1");
    assert_eq!(step.step.kind.to_string(), DOCKER_ACTION_KIND);
    let config = &step.step.config;
    assert_eq!(config["image"], json!({ "registry": "alpine:3.21" }));
    assert_eq!(config["entrypoint"], "/bin/echo");
    // One string, shell-split by the step after expressions resolve.
    assert!(config["args_text"].get("$expr").is_some());
    assert!(config.get("args").is_none());
    // Every `with:` key passes through as an input.
    assert!(config["inputs"]["who"].get("$expr").is_some());
}

#[test]
fn a_manifest_docker_action_places_pre_and_post_and_binds_inputs() {
    let source = MapActionSource::new().with("acme/publish@v1", TEST_SHA, DOCKER_MANIFEST);
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/publish@v1
        with:
          message: from-caller
          token: ${{ secrets.PUBLISH_TOKEN }}
      - run: echo after
"#;
    let graph = lower(text, &source).graph.expect("lowers");
    let names: Vec<&str> = graph
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .filter(|n| !n.ends_with("/start") && !n.ends_with("/done"))
        .collect();
    assert_eq!(names, vec![
        "build/step-1/pre",
        "build/step-1",
        "build/step-2",
        "build/step-1/post",
    ]);

    let main = node(&graph, "build/step-1");
    assert_eq!(main.step.kind.to_string(), DOCKER_ACTION_KIND);
    let config = &main.step.config;
    assert_eq!(config["image"]["dockerfile"]["file"], "Dockerfile");
    assert_eq!(config["image"]["dockerfile"]["action"]["sha"], TEST_SHA);
    assert_eq!(config["entrypoint"], "/entry.sh");
    // The manifest's args, the first bound to the caller's input.
    let args = config["args"].as_array().expect("args");
    assert_eq!(args.len(), 2);
    assert!(args[0].get("$expr").is_some());
    assert_eq!(args[1], "literal");
    // The manifest's own env, bound the same way.
    assert!(config["env"]["GREETING"].get("$expr").is_some());
    // Main's state comes from pre, as a JavaScript action's would.
    assert!(config.get("state").is_some());

    let pre = node(&graph, "build/step-1/pre");
    assert_eq!(pre.step.config["entrypoint"], "/pre.sh");
    assert!(pre.step.config.get("args").is_none(), "pre runs no args");

    let post = node(&graph, "build/step-1/post");
    assert_eq!(post.step.config["entrypoint"], "/post.sh");
    assert!(post.step.config.get("state").is_some());
}

#[test]
fn a_local_docker_action_builds_from_the_repository() {
    let files = MapFiles(
        [(
            ".github/actions/box/action.yml".to_string(),
            "runs:\n  using: docker\n  image: Dockerfile\n".to_string(),
        )]
        .into_iter()
        .collect(),
    );
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: ./.github/actions/box
"#;
    let lowered = load(".github/workflows/ci.yml", text, &files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = lowered.graph.expect("lowers");
    let step = node(&graph, "build/step-1");
    assert_eq!(step.step.kind.to_string(), DOCKER_ACTION_KIND);
    assert_eq!(
        step.step.config["image"]["dockerfile"]["action"],
        json!({ "local": ".github/actions/box" })
    );
}

#[test]
fn a_missing_required_input_warns_and_still_lowers() {
    let source = MapActionSource::new().with("acme/publish@v1", TEST_SHA, DOCKER_MANIFEST);
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/publish@v1
"#;
    let lowered = lower(text, &source);
    // GitHub's runner warns about a missing required input and runs anyway;
    // real workflows rely on that.
    assert!(lowered.graph.is_some());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "gha.missing_input"),
        "the missing required input is reported"
    );
}

#[test]
fn a_docker_step_inside_a_composite_lowers_its_main_phase() {
    let composite = r#"
runs:
  using: composite
  steps:
    - uses: docker://alpine:3.21
      with:
        args: echo inner
"#;
    let source = MapActionSource::new().with("acme/wrap@v1", TEST_SHA, composite);
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/wrap@v1
"#;
    let graph = lower(text, &source).graph.expect("lowers");
    let inner = node(&graph, "build/step-1/step-1");
    assert_eq!(inner.step.kind.to_string(), DOCKER_ACTION_KIND);
    assert_eq!(
        inner.step.config["image"],
        json!({ "registry": "alpine:3.21" })
    );
}
