//! Handoff §7 test 8 and §6: secrets reach the process and nothing else.

mod support;

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use driver::RunConfig;
use executor::{MapSecrets, Retention};
use ir::{GraphBuilder, RunStatus, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;
use tokio::time;

const SECRET: &str = "sk-live-9f3a2b7c1d4e";

/// §7 test 8. A step echoes a secret and round-trips it through its outputs
/// file. The log shows `***`, the output is masked too, and the raw event log
/// holds only the reference — checked by grepping the log's bytes for the
/// value.
#[tokio::test]
async fn a_secret_reaches_the_process_and_nothing_else() {
    let dir = RunDir::new("secrets");

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(
        "deploy",
        scope,
        StepRef::new(
            PROCESS_KIND,
            json!({
                "run": r#"echo "token is $DEPLOY_TOKEN"; echo "echoed=$DEPLOY_TOKEN" > "$CI_OUTPUT""#,
                "env": { "DEPLOY_TOKEN": { "$secret": "DEPLOY_TOKEN" } }
            }),
        ),
    );
    let graph = b.build();
    validate(&graph).expect("valid");

    let secrets = MapSecrets::from_pairs(&[("DEPLOY_TOKEN", SECRET)]);
    let report = host_driver_with(
        graph,
        &dir,
        secrets,
        RunConfig::new(dir.path()).with_retention(Retention::Never),
    )
    .await_run()
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    // The step really did receive the value: it echoed something, and what came
    // back is masked rather than empty.
    let lines = log_lines(&report);
    assert!(
        lines.iter().any(|l| l == "token is ***"),
        "the log line is masked: {lines:?}"
    );

    // The value round-tripped through the outputs file and is masked there too.
    assert_eq!(
        output_of(&report, "deploy")["echoed"],
        json!("***"),
        "output strings are masked before the finish record is appended"
    );

    // The decisive check: grep the bytes of everything the engine persists — the
    // event log, and the whole state, which carries the graph and so the configs.
    let log_bytes = serde_json::to_string(&report.state.log).expect("encode");
    assert!(
        !log_bytes.contains(SECRET),
        "the secret value leaked into the event log"
    );
    let state_bytes = serde_json::to_string(&report.state).expect("encode");
    assert!(
        !state_bytes.contains(SECRET),
        "the secret value leaked into the persisted state"
    );

    // What is persisted is the reference, by name.
    assert!(
        state_bytes.contains("$secret"),
        "the reference is what crossed the boundary"
    );
    assert!(state_bytes.contains("DEPLOY_TOKEN"), "by name");

    // Nor on disk, in the persisted step log.
    let log_dir = dir.logs();
    let mut found_masked = false;
    if let Ok(entries) = fs::read_dir(&log_dir) {
        for entry in entries.flatten() {
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            assert!(
                !text.contains(SECRET),
                "the secret leaked into {:?}",
                entry.path()
            );
            found_masked |= text.contains("***");
        }
    }
    assert!(found_masked, "the persisted log was written, and masked");
}

/// A short secret is not masked, GHA-style: masking `ok` would turn every log
/// into asterisks.
#[tokio::test]
async fn short_secrets_are_not_masked() {
    let dir = RunDir::new("short-secret");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(
        "show",
        scope,
        StepRef::new(
            PROCESS_KIND,
            json!({
                "run": r#"echo "value is $TINY""#,
                "env": { "TINY": { "$secret": "TINY" } }
            }),
        ),
    );
    let graph = b.build();

    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::from_pairs(&[("TINY", "ok")]),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(log_lines(&report).iter().any(|l| l == "value is ok"));
}

/// A `$secret` outside an env-shaped position is a step failure, not something
/// to pass through as literal JSON.
#[tokio::test]
async fn a_misplaced_secret_reference_fails_the_step() {
    let dir = RunDir::new("misplaced-secret");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(
        "sneaky",
        scope,
        StepRef::new(
            PROCESS_KIND,
            json!({ "run": "echo hello", "working_dir": { "$secret": "DEPLOY_TOKEN" } }),
        ),
    );
    let graph = b.build();

    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::from_pairs(&[("DEPLOY_TOKEN", SECRET)]),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
    )
    .await_run()
    .await;

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "sneaky")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some(steps::SECRET_MISPLACED_CLASS)
    );
}

/// A secret that does not exist fails the step rather than passing an empty
/// value.
#[tokio::test]
async fn an_unknown_secret_fails_the_step() {
    let dir = RunDir::new("unknown-secret");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(
        "deploy",
        scope,
        StepRef::new(
            PROCESS_KIND,
            json!({
                "run": "echo hello",
                "env": { "TOKEN": { "$secret": "NOT_CONFIGURED" } }
            }),
        ),
    );
    let graph = b.build();

    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "deploy")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some(steps::SECRET_UNAVAILABLE_CLASS)
    );
}

/// A step kind that resolves a secret and then leaks it through every progress
/// shape: a log line, an artifact's name and uri, and a custom payload.
struct LeakyStep;

const LEAKY_KIND: ir::StepKindId = ir::StepKindId::new_static("leaky");

impl ir::StepKind for LeakyStep {
    fn id(&self) -> ir::StepKindId {
        LEAKY_KIND
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the trait fixes this signature; an impl cannot widen the returned lifetime"
    )]
    fn name(&self) -> &str {
        "leaky"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for LeakyStep {
    async fn run(&self, ctx: steps::StepCtx) -> ir::Outcome {
        let token = ctx
            .secrets
            .resolve("DEPLOY_TOKEN")
            .expect("configured")
            .expose();
        let _ = ctx
            .logs
            .send(ir::StepEvent::Artifact {
                name: smol_str::SmolStr::new(format!("report-{token}")),
                uri:  format!("s3://bucket/{token}/report.tgz"),
            })
            .await;
        let _ = ctx
            .logs
            .send(ir::StepEvent::Custom(json!({ "token": token.as_str() })))
            .await;
        // Progress rides its own pump task; give it time to reach the driver
        // before the finish signal races it there.
        time::sleep(Duration::from_millis(200)).await;
        ir::Outcome::success(json!("done"))
    }
}

/// Exact-value masking covers every progress shape, `Artifact` included: a
/// registered value in an artifact's name or uri is `***` in the log.
#[tokio::test]
async fn an_artifact_carrying_a_registered_value_is_masked() {
    let dir = RunDir::new("artifact-secret");
    let mut b = GraphBuilder::new();
    b.add_node(
        "leak",
        ScopeId::new(0),
        StepRef::new(LEAKY_KIND, serde_json::Value::Null),
    );
    let graph = b.build();

    let mut registry = runners();
    registry.register_runner(Arc::new(LeakyStep));
    let report = host_driver_full(
        graph,
        &dir,
        MapSecrets::from_pairs(&[("DEPLOY_TOKEN", SECRET)]),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
        registry,
    )
    .await_run()
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let artifacts: Vec<(String, String)> = report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgress {
                ev: ir::StepEvent::Artifact { name, uri },
                ..
            } => Some((name.to_string(), uri.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(artifacts, vec![(
        "report-***".into(),
        "s3://bucket/***/report.tgz".into()
    )]);

    let log_bytes = serde_json::to_string(&report.state.log).expect("encode");
    assert!(
        !log_bytes.contains(SECRET),
        "the secret leaked into the event log"
    );
}

/// A step kind whose failure message embeds a resolved secret — the shape a
/// message takes when it quotes raw stderr or an unparseable output line. The
/// config picks the status: a hard failure, or a soft one that keeps the
/// failure in `underlying`.
struct FailLeakyStep;

const FAIL_LEAKY_KIND: ir::StepKindId = ir::StepKindId::new_static("fail-leaky");

impl ir::StepKind for FailLeakyStep {
    fn id(&self) -> ir::StepKindId {
        FAIL_LEAKY_KIND
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the trait fixes this signature; an impl cannot widen the returned lifetime"
    )]
    fn name(&self) -> &str {
        "fail-leaky"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for FailLeakyStep {
    async fn run(&self, ctx: steps::StepCtx) -> ir::Outcome {
        let token = ctx
            .secrets
            .resolve("DEPLOY_TOKEN")
            .expect("configured")
            .expose();
        let info = ir::FailureInfo::new(format!("upload refused: token {token} rejected"))
            .with_class("upload_refused");
        let status = if ctx.config.as_str() == Some("partial") {
            ir::Status::partial(info)
        } else {
            ir::Status::Failure(info)
        };
        ir::Outcome::new(status, ir::Value::Null)
    }
}

/// Failure messages are masked at `finish` like the output is: a step that
/// quotes a resolved secret in its failure — or in the `underlying` failure a
/// soft fail keeps — persists `***`, and the event log never holds the value.
/// The class is untouched: routing matches on it.
#[tokio::test]
async fn a_failure_message_carrying_a_secret_is_masked() {
    let dir = RunDir::new("failure-secret");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node("hard", scope, StepRef::new(FAIL_LEAKY_KIND, json!("fail")));
    b.add_node(
        "soft",
        scope,
        StepRef::new(FAIL_LEAKY_KIND, json!("partial")),
    );
    let graph = b.build();

    let mut registry = runners();
    registry.register_runner(Arc::new(FailLeakyStep));
    let report = host_driver_full(
        graph,
        &dir,
        MapSecrets::from_pairs(&[("DEPLOY_TOKEN", SECRET)]),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
        registry,
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Failed);

    let info_of = |name: &str| {
        report
            .state
            .history()
            .iter()
            .find(|r| r.name == name)
            .expect("the node recorded")
            .outcome
            .status
            .failure_info()
            .cloned()
            .expect("the failure is on the record")
    };
    let hard = info_of("hard");
    assert_eq!(hard.message, "upload refused: token *** rejected");
    assert_eq!(hard.class, "upload_refused", "the class is never masked");
    let soft = info_of("soft");
    assert_eq!(
        soft.message, "upload refused: token *** rejected",
        "the underlying failure of a soft fail is masked too"
    );

    let log_bytes = serde_json::to_string(&report.state.log).expect("encode");
    assert!(
        !log_bytes.contains(SECRET),
        "the secret value leaked into the event log"
    );
    assert!(log_bytes.contains("***"), "the mask is what was persisted");
}

/// A multi-line secret is masked line by line, because the log is
/// line-buffered.
#[tokio::test]
async fn multiline_secrets_are_masked_per_line() {
    let dir = RunDir::new("multiline-secret");
    let key = "-----BEGIN KEY-----\nabcdefghijklmnop\nqrstuvwxyz012345\n-----END KEY-----";

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(
        "show",
        scope,
        StepRef::new(
            PROCESS_KIND,
            json!({
                "run": r#"echo "$PRIVATE_KEY""#,
                "env": { "PRIVATE_KEY": { "$secret": "PRIVATE_KEY" } }
            }),
        ),
    );
    let graph = b.build();

    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::from_pairs(&[("PRIVATE_KEY", key)]),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Success);

    let lines = log_lines(&report);
    for secret_line in key.lines() {
        assert!(
            !lines.iter().any(|l| l.contains(secret_line)),
            "a line of the key survived: {secret_line}"
        );
    }
    assert!(lines.iter().any(|l| l == "***"), "{lines:?}");
}
