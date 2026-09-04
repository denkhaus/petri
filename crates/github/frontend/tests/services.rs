//! `services:` lowering: sidecar containers onto the job's scope, realized at
//! acquisition — image, env, and `options` typed at lowering; `ports` warned
//! about and dropped.

use frontend::NoFiles;
use frontend_gha::load;

#[expect(
    clippy::print_stderr,
    reason = "a failing test needs the lowering diagnostics on stderr to be readable"
)]
fn lower(text: &str) -> frontend::Lowered {
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

#[test]
fn services_land_on_the_scope() {
    let text = r"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:15-alpine
        env:
          POSTGRES_DB: django
          POSTGRES_PASSWORD: postgres
        ports:
          - 5432:5432
        options: >-
          --health-cmd pg_isready
          --health-interval 10s
    steps:
      - run: echo hi
";
    let graph = lower(text).graph.expect("lowers");
    let scope = graph
        .scopes
        .iter()
        .find(|s| !s.services.is_empty())
        .expect("the job scope carries the service");
    assert_eq!(scope.services.len(), 1);
    let service = &scope.services[0];
    assert_eq!(service.name, "postgres");
    assert_eq!(service.image, "postgres:15-alpine");
    assert_eq!(
        service.env.get("POSTGRES_DB"),
        Some(&ir::ExprOrValue::Value(serde_json::json!("django")))
    );
    // Options are typed at lowering: the health check arrives as fields.
    let health = service.options.health.as_ref().expect("the health check");
    assert_eq!(health.cmd.as_deref(), Some("pg_isready"));
    assert_eq!(health.interval_ms, Some(10_000));
}

/// `ports:` is accepted and dropped with a warning that names the service:
/// the job reaches a service by its name on the scope's network, and no
/// port is published to the host. The graph still lowers.
#[test]
fn service_ports_are_warned_about_and_dropped() {
    let text = r"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:15-alpine
        ports:
          - 5432:5432
    steps:
      - run: echo hi
";
    let lowered = lower(text);
    assert!(lowered.graph.is_some(), "ports do not reject the workflow");
    let warning = lowered
        .diagnostics
        .iter()
        .find(|d| d.code == "ignored.services.ports")
        .unwrap_or_else(|| panic!("{:?}", lowered.diagnostics.iter().collect::<Vec<_>>()));
    assert_eq!(warning.severity, frontend::Severity::Warning);
    assert!(warning.message.contains("postgres"), "{}", warning.message);
}

/// An unknown service flag is rejected at lowering, naming the service and
/// the flag.
#[test]
fn an_unknown_service_option_is_rejected_by_name() {
    let text = r"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      redis:
        image: redis:7-alpine
        options: --memory 1g
    steps:
      - run: echo hi
";
    let lowered = lower(text);
    assert!(lowered.graph.is_none());
    let rejection = lowered
        .diagnostics
        .iter()
        .find(|d| d.code == "unsupported.container.option")
        .expect("the unknown flag is rejected");
    assert!(
        rejection.message.contains("--memory") && rejection.message.contains("redis"),
        "{}",
        rejection.message
    );
}

#[test]
fn an_expression_image_is_rejected_specifically() {
    let text = r"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        version: [15, 16]
    services:
      postgres:
        image: postgres:${{ matrix.version }}-alpine
    steps:
      - run: echo hi
";
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
fn a_secret_service_env_is_rejected_specifically() {
    let text = r"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      db:
        image: postgres:15-alpine
        env:
          POSTGRES_PASSWORD: ${{ secrets.DB_PASSWORD }}
    steps:
      - run: echo hi
";
    let lowered = lower(text);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "unsupported.services.secret_env"),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}
