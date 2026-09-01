//! Join-time publication for GitHub background steps.

use ir::{FailureClass, FailureInfo, LogStream, Outcome, Status, Value};
use serde::{Deserialize, Serialize};
use steps::{Step, StepCtx};

use crate::session::{initialize_background, publish_background};

const BACKGROUND_CLASS: FailureClass = FailureClass::new_static("background_step");

/// Snapshots the foreground job environment before the branch and foreground
/// fan out. This makes the launch point deterministic under task scheduling.
pub struct BackgroundStartStep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartConfig {
    key:             String,
    #[serde(default)]
    job_environment: Option<String>,
}

#[async_trait::async_trait]
impl Step for BackgroundStartStep {
    const NAME: &'static str = frontend_gha::BACKGROUND_START_KIND;
    type Config = StartConfig;

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome {
        match initialize_background(&*ctx.env, &config.key, config.job_environment.as_deref()).await
        {
            Ok(()) => Outcome::success(Value::Null),
            Err(failure) => failure.into(),
        }
    }
}

/// The private terminal of one background branch. Its successful record keeps
/// the worker's real status and output inert until a wait publishes them.
pub struct BackgroundCompleteStep;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundResult {
    status:          String,
    output:          Value,
    key:             String,
    #[serde(default)]
    job_environment: Option<String>,
}

#[async_trait::async_trait]
impl Step for BackgroundCompleteStep {
    const NAME: &'static str = frontend_gha::BACKGROUND_COMPLETE_KIND;
    type Config = BackgroundResult;

    async fn run(&self, config: Self::Config, _ctx: StepCtx) -> Outcome {
        Outcome::success(
            serde_json::to_value(config).expect("a background result is JSON-serializable"),
        )
    }
}

/// Applies a branch's deferred environment effects and reproduces its result
/// on the public step node (or an internal re-publication node).
pub struct BackgroundPublishStep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishConfig {
    result: BackgroundResult,
}

#[async_trait::async_trait]
impl Step for BackgroundPublishStep {
    const NAME: &'static str = frontend_gha::BACKGROUND_PUBLISH_KIND;
    type Config = PublishConfig;

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome {
        let result = config.result;
        let mut status = status_from_tag(&result.status);
        if let Err(failure) =
            publish_background(&*ctx.env, &result.key, result.job_environment.as_deref()).await
            && status.is_success_like()
        {
            status = Status::Failure(FailureInfo::new(failure.message).with_class(failure.class));
        }
        Outcome::new(status, result.output)
    }
}

/// The control step at a `wait`, `wait-all`, or implicit barrier. Public
/// background records tolerate their own failures; this node is where the
/// included conclusions affect the foreground job status.
pub struct BackgroundWaitStep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitConfig {
    #[serde(default)]
    targets:           Vec<WaitTarget>,
    #[serde(default)]
    continue_on_error: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitTarget {
    name:            String,
    status:          String,
    #[serde(default)]
    timeout_minutes: Option<String>,
}

#[async_trait::async_trait]
impl Step for BackgroundWaitStep {
    const NAME: &'static str = frontend_gha::BACKGROUND_WAIT_KIND;
    type Config = WaitConfig;

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome {
        let mut failed = false;
        let mut cancelled = false;
        for target in &config.targets {
            let verdict = match target.status.as_str() {
                "cancelled" => {
                    cancelled = true;
                    "Canceled"
                }
                "skipped" => "Skipped",
                "success" | "partial_success" => "Succeeded",
                "timed_out" => {
                    failed = true;
                    if let Some(minutes) = &target.timeout_minutes {
                        ctx.log(
                            LogStream::Stdout,
                            format!(
                                "The background step '{}' has timed out after {minutes} minutes",
                                target.name
                            ),
                        )
                        .await;
                    }
                    "Failed"
                }
                _ => {
                    failed = true;
                    "Failed"
                }
            };
            ctx.log(LogStream::Stdout, format!("{}: {verdict}", target.name))
                .await;
        }
        if failed {
            let failure = FailureInfo::new("one or more background steps failed")
                .with_class(BACKGROUND_CLASS);
            return if config.continue_on_error {
                Outcome::partial(failure, Value::Null)
            } else {
                Outcome::new(Status::Failure(failure), Value::Null)
            };
        }
        if cancelled {
            return Outcome::cancelled();
        }
        Outcome::success(Value::Null)
    }
}

fn status_from_tag(tag: &str) -> Status {
    Status::from_tag(tag, || {
        FailureInfo::new("the background step failed").with_class(BACKGROUND_CLASS)
    })
    .unwrap_or_else(|| {
        Status::Failure(
            FailureInfo::new(format!("unknown background status `{tag}`"))
                .with_class(BACKGROUND_CLASS),
        )
    })
}
