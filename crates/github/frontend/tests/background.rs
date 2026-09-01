mod support;

use frontend_gha::{
    BACKGROUND_COMPLETE_KIND, BACKGROUND_PUBLISH_KIND, BACKGROUND_WAIT_KIND, RUN_KIND,
};
use support::*;

#[test]
fn background_steps_fan_out_and_publish_at_waits() {
    let graph = lower_ok(
        r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - id: server
        name: Server ${{ matrix.name }}
        background: true
        run: sleep 1
      - run: echo foreground
      - wait: server
      - id: tail
        background: true
        run: echo tail
      - run: echo after
",
    );
    ir::validate(&graph).expect("valid background graph");

    let worker = graph
        .nodes
        .iter()
        .find(|node| node.name == "build/server/background")
        .expect("private worker");
    assert_eq!(worker.step.kind.as_ref(), RUN_KIND);
    assert_eq!(worker.step.config["background"], "background-1");
    assert!(worker.tolerates_failure);

    let complete = graph
        .nodes
        .iter()
        .find(|node| node.name == "build/server/background-done")
        .expect("private completion");
    assert_eq!(complete.step.kind.as_ref(), BACKGROUND_COMPLETE_KIND);
    let public = graph
        .nodes
        .iter()
        .find(|node| node.name == "build/server")
        .expect("join-time public record");
    assert_eq!(public.step.kind.as_ref(), BACKGROUND_PUBLISH_KIND);
    assert!(public.tolerates_failure);

    let waits: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| node.step.kind.as_ref() == BACKGROUND_WAIT_KIND)
        .collect();
    assert_eq!(waits.len(), 2, "explicit wait plus implicit tail barrier");
    assert!(
        graph
            .node(waits[0].id)
            .is_some_and(|node| node.step.config["targets"].as_array().unwrap().len() == 1)
    );
}

#[test]
fn parallel_desugars_to_background_members_and_one_wait() {
    let graph = lower_ok(
        r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - parallel:
          - id: one
            run: echo one
          - uses: docker://alpine:3.20
            with:
              args: echo two
      - run: echo joined
",
    );
    ir::validate(&graph).expect("valid parallel graph");
    assert!(graph.nodes.iter().any(|node| node.name == "build/one"));
    assert!(graph.nodes.iter().any(|node| node.name == "build/step-1-2"));
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| node.step.kind.as_ref() == BACKGROUND_WAIT_KIND)
            .count(),
        1
    );
}

#[test]
fn invalid_background_shapes_are_diagnostics() {
    let diagnostics = diagnostics(
        r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - wait: later
        if: always()
      - id: later
        background: maybe
        run: echo later
      - cancel: later
",
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "gha.bad_step")
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unsupported.step.cancel")
    );
}

#[test]
fn matrix_background_channels_are_isolated_per_leg() {
    let graph = lower_ok(
        r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        n: [1, 2]
    steps:
      - id: worker
        background: true
        run: echo ${{ matrix.n }}
      - wait: worker
",
    );
    ir::validate(&graph).expect("valid matrix background graph");
    let worker = graph
        .nodes
        .iter()
        .find(|node| node.name == "build/worker/background")
        .expect("worker template");
    assert!(
        worker.step.config["background"].get("$expr").is_some(),
        "the clone index is part of the private channel"
    );
    assert!(
        worker.step.config["job_environment"].get("$expr").is_some(),
        "the clone index also isolates the leg's foreground environment"
    );
}

#[test]
fn an_action_post_runs_after_the_implicit_wait() {
    let action = r"
name: Lifecycle
runs:
  using: node24
  main: main.js
  post: post.js
";
    let workflow = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - id: tool
        background: true
        uses: ./.github/actions/tool
      - run: echo foreground
";
    let source = files(&[(".github/actions/tool/action.yml", action)]);
    let graph = lower_ok_with(workflow, &source);
    ir::validate(&graph).expect("valid lifecycle graph");
    let wait = graph
        .nodes
        .iter()
        .position(|node| node.step.kind.as_ref() == BACKGROUND_WAIT_KIND)
        .expect("implicit wait");
    let post = graph
        .nodes
        .iter()
        .position(|node| node.name == "build/tool/post-resolve")
        .expect("post dispatcher");
    assert!(
        wait < post,
        "the implicit wait is created before deferred post cleanup"
    );
    assert!(
        graph.nodes[post].step.config["request"]
            .get("$expr")
            .is_some()
    );
}

#[test]
fn explicit_waits_require_an_earlier_background_id() {
    let diagnostics = diagnostics(
        r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - background: true
        run: echo anonymous
      - wait: step-1
",
    );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "gha.bad_step"
            && diagnostic
                .message
                .contains("does not name an earlier background step")
    }));
}

#[test]
fn composite_background_forms_are_checked_when_the_action_runs() {
    let action = r"
runs:
  using: composite
  steps:
    - background: true
      shell: bash
      run: echo no
    - wait-all:
";
    let workflow = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: ./.github/actions/tool
";
    let source = files(&[(".github/actions/tool/action.yml", action)]);
    let lowered = frontend_gha::load(".github/workflows/test.yml", workflow, &source);
    assert!(
        !lowered
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unsupported.step.wait_composite"),
        "the manifest is not read by static lowering"
    );
    let graph = lowered.graph.expect("the deferred action lowers");
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.name == "build/step-1/resolve")
    );
}
