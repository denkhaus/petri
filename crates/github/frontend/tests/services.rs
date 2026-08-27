//! `services:` lowering: sidecar containers onto the job's scope, realized at
//! acquisition — image, env, ports, and `options` split into the flags GitHub
//! hands the engine.

use frontend::NoFiles;
use frontend_gha::load;

fn lower(text: &str) -> frontend::Lowered {
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

#[test]
fn services_land_on_the_scope() {
    let text = r#"
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
"#;
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
    assert_eq!(service.ports, vec!["5432:5432"]);
    assert_eq!(
        service.options,
        vec![
            "--health-cmd",
            "pg_isready",
            "--health-interval",
            "10s",
        ]
    );
}

#[test]
fn an_expression_image_is_rejected_specifically() {
    let text = r#"
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
fn a_secret_service_env_is_rejected_specifically() {
    let text = r#"
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
"#;
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
