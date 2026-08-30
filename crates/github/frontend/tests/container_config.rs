//! Container config lowering: `container.options` as raw engine flags,
//! `container.credentials` as a username plus a secret *name*, and
//! expression-valued images resolved through the static contexts.

use frontend::NoFiles;
use frontend_gha::load;
use ir::RuntimeTarget;

fn lower(text: &str) -> frontend::Lowered {
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

fn container_scope(graph: &ir::Graph) -> &ir::Scope {
    graph
        .scopes
        .iter()
        .find(|s| matches!(s.runtime.target, RuntimeTarget::Container { .. }))
        .expect("a container scope")
}

#[test]
fn options_and_credentials_land_on_the_target() {
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: ghcr.io/acme/builder:1
      options: --cpus 2 --health-cmd "curl -f localhost"
      credentials:
        username: robot
        password: ${{ secrets.GHCR_TOKEN }}
    steps:
      - run: echo hi
"#;
    let graph = lower(text).graph.expect("lowers");
    let scope = container_scope(&graph);
    let RuntimeTarget::Container {
        image,
        options,
        credentials,
    } = &scope.runtime.target
    else {
        unreachable!()
    };
    assert_eq!(image, "ghcr.io/acme/builder:1");
    assert_eq!(options, &[
        "--cpus",
        "2",
        "--health-cmd",
        "curl -f localhost"
    ]);
    let credentials = credentials.as_ref().expect("credentials");
    assert_eq!(credentials.username, "robot");
    // The graph carries the secret's *name*, never a value.
    assert_eq!(credentials.password_secret, "GHCR_TOKEN");
}

#[test]
fn container_env_lands_on_the_scope_and_its_secrets_push_down() {
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: alpine:3.20
      env:
        NODE_ENV: production
        TOKEN: ${{ secrets.API_TOKEN }}
    env:
      NODE_ENV: from-the-job
    steps:
      - run: echo hi
"#;
    let graph = lower(text).graph.expect("lowers");
    let scope = container_scope(&graph);
    // Job env wins over container env for the shared scope map.
    assert_eq!(
        scope.env.get("NODE_ENV"),
        Some(&ir::ExprOrValue::Value(serde_json::json!("from-the-job")))
    );
    // The secret is pushed down into the step's env, never onto the scope.
    assert!(!scope.env.contains_key("TOKEN"));
    let step = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-1")
        .expect("the step");
    assert_eq!(
        step.step.config["env"]["TOKEN"],
        serde_json::json!({ "$secret": "API_TOKEN" })
    );
}

#[test]
fn an_input_valued_image_resolves_at_lowering() {
    let text = r#"
on:
  workflow_dispatch:
    inputs:
      base:
        default: alpine
jobs:
  build:
    runs-on: ubuntu-latest
    container: ${{ inputs.base }}:3.20
    steps:
      - run: echo hi
"#;
    let graph = lower(text).graph.expect("lowers");
    let scope = container_scope(&graph);
    assert!(
        matches!(&scope.runtime.target, RuntimeTarget::Container { image, .. } if image == "alpine:3.20")
    );
}

#[test]
fn a_matrix_valued_image_stays_rejected() {
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        base: [alpine, debian]
    container: ${{ matrix.base }}:latest
    steps:
      - run: echo hi
"#;
    let lowered = lower(text);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "unsupported.container.expression"),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn a_non_secret_password_is_rejected() {
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: ghcr.io/acme/builder:1
      credentials:
        username: robot
        password: hunter2
    steps:
      - run: echo hi
"#;
    let lowered = lower(text);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "unsupported.container.credentials"),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn service_credentials_map_the_same_way() {
    let text = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      db:
        image: registry.example.com/acme/db:1
        credentials:
          username: robot
          password: ${{ secrets.REGISTRY_TOKEN }}
    steps:
      - run: echo hi
"#;
    let graph = lower(text).graph.expect("lowers");
    let scope = graph
        .scopes
        .iter()
        .find(|s| !s.services.is_empty())
        .expect("the service scope");
    let credentials = scope.services[0].credentials.as_ref().expect("credentials");
    assert_eq!(credentials.username, "robot");
    assert_eq!(credentials.password_secret, "REGISTRY_TOKEN");
}
