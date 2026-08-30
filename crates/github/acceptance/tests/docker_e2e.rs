//! Docker container actions end to end: `uses: docker://…` and a local
//! Dockerfile action, run for real against the daemon — in a host job and in a
//! containerized job. These skip when no daemon is reachable
//! (`PETRI_REQUIRE_DOCKER` turns the skip into a failure).

mod support;

use runtime::ir;
use runtime::ir::RunStatus;
use support::*;
use testkit::docker_ready;

fn output_of(report: &RunReportPlus, name: &str) -> ir::Value {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome.output.clone())
        .unwrap_or(ir::Value::Null)
}

/// A `docker://` step runs in the daemon with the workspace mounted, sees the
/// scope's `GITHUB_*` env and its `INPUT_*`, writes `GITHUB_OUTPUT`, and a
/// later step of the (host) job reads the output back.
#[tokio::test]
async fn a_docker_url_action_runs_in_a_host_job() {
    if !docker_ready().await {
        return;
    }
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - id: box
        uses: docker://alpine:3.20
        with:
          who: world
          args: sh -c 'echo "hello $INPUT_WHO from $GITHUB_REPOSITORY"; echo "answer=42" >> "$GITHUB_OUTPUT"'
      - run: echo "carried ${{ steps.box.outputs.answer }}"
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "docker-url-action").await;
    assert_eq!(report.status, RunStatus::Success, "{:?}", errors(&report));
    let lines = log_lines(&report);
    assert!(
        lines.iter().any(|l| l == "hello world from example/repo"),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l == "carried 42"), "{lines:?}");
    assert_eq!(output_of(&report, "build/box")["answer"], "42");
}

/// A local Dockerfile action builds from the checked-out repository and runs.
/// The Dockerfile only has to exist at run time — GitHub resolves it against
/// the workspace — so an earlier step writes it.
#[tokio::test]
async fn a_local_dockerfile_action_builds_and_runs() {
    if !docker_ready().await {
        return;
    }
    let files = files(&[(
        ".github/actions/box/action.yml",
        r#"
inputs:
  greeting:
    default: built
runs:
  using: docker
  image: Dockerfile
  args:
    - ${{ inputs.greeting }}
"#,
    )]);
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: |
          mkdir -p .github/actions/box
          printf 'FROM alpine:3.20\nENTRYPOINT ["/bin/echo", "dockerfile-action-ran:"]\n' > .github/actions/box/Dockerfile
      - uses: ./.github/actions/box
        with:
          greeting: and-spoke
"#;
    let graph = lower_ok_with(text, &files);
    let report = run_host(graph, "docker-local-action").await;
    assert_eq!(report.status, RunStatus::Success, "{:?}", errors(&report));
    let lines = log_lines(&report);
    assert!(
        lines
            .iter()
            .any(|l| l == "dockerfile-action-ran: and-spoke"),
        // The entrypoint echoes its appended args: the manifest's one arg,
        // bound to the caller's input.
        "{lines:?}"
    );
}

/// A `docker://` step inside a containerized job: the action container attaches
/// to the job container's world and shares its workspace, so a file the job
/// wrote is visible to the action and the action's output comes back.
#[tokio::test]
async fn a_docker_action_runs_beside_a_containerized_job() {
    if !docker_ready().await {
        return;
    }
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container: alpine:3.20
    defaults:
      run:
        shell: sh
    steps:
      - run: echo from-the-job > note.txt
      - id: box
        uses: docker://alpine:3.20
        with:
          args: sh -c 'cat "$GITHUB_WORKSPACE/note.txt"'
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "docker-action-in-container-job").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}\n{:?}",
        errors(&report),
        log_lines(&report)
    );
    let lines = log_lines(&report);
    assert!(lines.iter().any(|l| l == "from-the-job"), "{lines:?}");
}

/// `container.options` pass through to the engine as raw flags: an env flag in
/// the options is visible to every step of the containerized job, and an
/// expression-valued image resolves at lowering through the static contexts.
#[tokio::test]
async fn container_options_and_a_static_image_expression_apply() {
    if !docker_ready().await {
        return;
    }
    let text = r#"
on:
  workflow_dispatch:
    inputs:
      base:
        default: alpine
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: ${{ inputs.base }}:3.20
      options: -e PETRI_FROM_OPTIONS=carried
    steps:
      - run: echo "options say $PETRI_FROM_OPTIONS on $(cat /etc/alpine-release >/dev/null && echo alpine)"
        shell: sh
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "container-options").await;
    assert_eq!(report.status, RunStatus::Success, "{:?}", errors(&report));
    let lines = log_lines(&report);
    assert!(
        lines.iter().any(|l| l == "options say carried on alpine"),
        "{lines:?}"
    );
}

/// `services:` end to end, in both placements at once: the host job reaches
/// its service over the published port on localhost, and the containerized job
/// reaches its own by name — each healthy before the job's first step.
#[tokio::test]
async fn services_are_reachable_from_host_and_containerized_jobs() {
    if !docker_ready().await {
        return;
    }
    let text = r#"
on: push
jobs:
  on-host:
    runs-on: ubuntu-latest
    services:
      redis:
        image: redis:7-alpine
        ports:
          - 29812:6379
        options: >-
          --health-cmd "redis-cli ping"
          --health-interval 1s
          --health-timeout 3s
          --health-retries 30
    steps:
      - run: 'exec 3<>/dev/tcp/localhost/29812 && echo service-reachable'
  inside:
    runs-on: ubuntu-latest
    container: alpine:3.20
    services:
      redis:
        image: redis:7-alpine
    steps:
      - run: |
          tries=0
          until nslookup redis; do
            tries=$((tries + 1))
            [ "$tries" -ge 10 ] && exit 1
            sleep 1
          done
          echo service-resolved
        shell: sh
"#;
    let graph = lower_ok(text);
    let report = run_host(graph, "services-e2e").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}\n{:?}",
        errors(&report),
        log_lines(&report)
    );
    let lines = log_lines(&report);
    assert!(lines.iter().any(|l| l == "service-reachable"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "service-resolved"), "{lines:?}");
}

fn errors(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .history()
        .iter()
        .filter(|r| r.outcome.status.is_failure())
        .map(|r| format!("{}: {:?}", r.name, r.outcome))
        .collect()
}
