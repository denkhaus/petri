//! Docker container action lowering: `uses: docker://…` and `runs.using:
//! docker` become `github/docker_action` nodes — main where the step is,
//! `pre-entrypoint` before every step, `post-entrypoint` after the last — with
//! the image, entrypoints and args carried in the config and the action's
//! inputs bound for the manifest's own expressions.

use frontend::{MapFiles, NoFiles};
use frontend_gha::action::MapActionSource;
use frontend_gha::{DEFERRED_ACTION_KIND, DOCKER_ACTION_KIND, load, load_with};
use serde_json::json;

const TEST_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

const DOCKER_MANIFEST: &str = r"
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
";

#[expect(
    clippy::print_stderr,
    reason = "a failing test needs the lowering diagnostics on stderr to be readable"
)]
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
#[expect(
    clippy::print_stderr,
    reason = "a failing test needs the lowering diagnostics on stderr to be readable"
)]
fn a_docker_url_step_lowers_with_args_and_entrypoint() {
    let text = r"
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
";
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
fn a_manifest_docker_action_stays_deferred_with_its_caller_inputs() {
    let source = MapActionSource::new().with("acme/publish@v1", TEST_SHA, DOCKER_MANIFEST);
    let text = r"
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
";
    let graph = lower(text, &source).graph.expect("lowers");
    let names: Vec<&str> = graph
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .filter(|n| !n.ends_with("/start") && !n.ends_with("/done"))
        .collect();
    assert_eq!(names, vec![
        "build/step-1/resolve",
        "build/step-1",
        "build/step-2",
        "build/step-1/post-resolve",
    ]);

    let resolver = node(&graph, "build/step-1/resolve");
    assert_eq!(resolver.step.kind.to_string(), DEFERRED_ACTION_KIND);
    let config = &resolver.step.config;
    assert_eq!(config["action"]["sha"], TEST_SHA);
    assert_eq!(config["with"]["message"], "from-caller");
    assert_eq!(
        config["with"]["token"],
        json!({ "$secret": "PUBLISH_TOKEN" })
    );
    assert!(config.get("image").is_none(), "the manifest is still lazy");
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a failing test needs the lowering diagnostics on stderr to be readable"
)]
fn a_local_docker_action_builds_from_the_repository() {
    let files = MapFiles(
        [(
            ".github/actions/box/action.yml".to_string(),
            "runs:\n  using: docker\n  image: Dockerfile\n".to_string(),
        )]
        .into_iter()
        .collect(),
    );
    let text = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: ./.github/actions/box
";
    let lowered = load(".github/workflows/ci.yml", text, &files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = lowered.graph.expect("lowers");
    let step = node(&graph, "build/step-1/resolve");
    assert_eq!(step.step.kind.to_string(), DEFERRED_ACTION_KIND);
    assert_eq!(
        step.step.config["action"],
        json!({ "local": ".github/actions/box" })
    );
}

#[test]
fn a_missing_required_input_warns_and_still_lowers() {
    let source = MapActionSource::new().with("acme/publish@v1", TEST_SHA, DOCKER_MANIFEST);
    let text = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/publish@v1
";
    let lowered = lower(text, &source);
    // The declaration is in the lazy manifest, so the run-time planner warns.
    assert!(lowered.graph.is_some());
    assert!(
        !lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "gha.missing_input"),
        "the static frontend has not read the input declarations"
    );
}

#[test]
fn a_docker_step_inside_a_composite_is_discovered_at_run_time() {
    let composite = r"
runs:
  using: composite
  steps:
    - uses: docker://alpine:3.21
      with:
        args: echo inner
";
    let source = MapActionSource::new().with("acme/wrap@v1", TEST_SHA, composite);
    let text = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/wrap@v1
";
    let graph = lower(text, &source).graph.expect("lowers");
    let resolver = node(&graph, "build/step-1/resolve");
    assert_eq!(resolver.step.kind.to_string(), DEFERRED_ACTION_KIND);
    assert!(
        graph
            .nodes
            .iter()
            .all(|node| node.name != "build/step-1/step-1"),
        "the composite manifest has not been expanded"
    );
}
