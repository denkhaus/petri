//! The process step kind: run a script, capture its output, honour cancellation.
//!
//! Written once against [`ExecEnv`](executor::ExecEnv). It never mentions Docker.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use executor::secrets::SECRET_REF_KEY;
use executor::{ExitStatus, LogLine, ProcessSpec, Sig};
use ir::{FailureInfo, Outcome, Status, StepEvent, StepKindId, Value};
use serde::Deserialize;
use serde_json::Map;
use smol_str::SmolStr;
use tokio::sync::mpsc;

use crate::ctx::{StepCtx, StepRunner};
use crate::outputs::{BAD_OUTPUT_CLASS, parse};

/// The step kind id the process step registers under.
pub const PROCESS_KIND: StepKindId = StepKindId::new(1);

/// The environment variable naming the outputs file.
pub const OUTPUT_ENV: &str = "CI_OUTPUT";

/// A `$secret` reference turned up somewhere it is not allowed.
pub const SECRET_MISPLACED_CLASS: &str = "secret_misplaced";

/// The step's config did not deserialize.
pub const BAD_CONFIG_CLASS: &str = "bad_config";

/// A named secret is not configured for this run.
pub const SECRET_UNAVAILABLE_CLASS: &str = "secret_unavailable";

/// The workspace could not be prepared for the step.
pub const WORKSPACE_CLASS: &str = "workspace_setup";

/// The process could not be started at all.
pub const SPAWN_CLASS: &str = "spawn_failed";

/// How long to keep draining log output after the process has gone.
const DRAIN_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    #[default]
    Bash,
    Sh,
}

impl Shell {
    /// Fail on error, and fail a pipeline on any stage's error.
    fn invocation(self) -> (&'static str, Vec<&'static str>) {
        match self {
            Shell::Bash => ("bash", vec!["-eo", "pipefail", "-c"]),
            Shell::Sh => ("sh", vec!["-e", "-c"]),
        }
    }
}

/// Which non-zero exits are soft failures.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SoftFail {
    All(bool),
    ExitStatuses(Vec<i32>),
    #[default]
    #[serde(skip)]
    Off,
}

impl SoftFail {
    fn matches(&self, code: i32) -> bool {
        match self {
            SoftFail::Off => false,
            SoftFail::All(all) => *all,
            SoftFail::ExitStatuses(codes) => codes.contains(&code),
        }
    }
}

/// Either a literal env value or a reference to a secret.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ValueOrSecretRef {
    /// `{"$secret": "NAME"}` — resolved at spawn, straight into the child's
    /// environment, never written to the log.
    Secret {
        #[serde(rename = "$secret")]
        name: SmolStr,
    },
    Literal(Value),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// The script text.
    pub run: String,
    #[serde(default)]
    pub shell: Shell,
    #[serde(default)]
    pub env: BTreeMap<SmolStr, ValueOrSecretRef>,
    /// Relative to the workspace root.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub soft_fail: SoftFail,
    /// Extra environment variables that name the same outputs file as `CI_OUTPUT`,
    /// so a frontend whose scripts already write to a differently named file needs no
    /// shim. (The GitHub Actions frontend sets `["GITHUB_OUTPUT"]`.)
    #[serde(default)]
    pub output_env_aliases: Vec<SmolStr>,
}

/// Runs a script as a process, as its own process group.
pub struct ProcessStep;

#[async_trait::async_trait]
impl StepRunner for ProcessStep {
    fn kind(&self) -> StepKindId {
        PROCESS_KIND
    }

    fn name(&self) -> &str {
        "process"
    }

    async fn run(&self, ctx: StepCtx) -> Outcome {
        match execute(ctx).await {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }
}

/// A step that failed before it could run, in the few bytes needed to say so.
///
/// The short-circuit arm used to be a whole `Outcome`, which made every caller pay
/// for the larger of two identical types for no benefit. This carries the class and
/// the message, and becomes an `Outcome` once, at the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepFailure {
    pub class: &'static str,
    pub message: String,
}

impl From<StepFailure> for Outcome {
    fn from(failure: StepFailure) -> Self {
        Outcome::new(
            Status::Failure(FailureInfo::new(failure.message).with_class(failure.class)),
            Value::Null,
        )
    }
}

async fn execute(mut ctx: StepCtx) -> Result<Outcome, StepFailure> {
    // A `$secret` outside an env-shaped position is a step failure, not something to
    // quietly ignore: it would otherwise reach the process as the literal JSON.
    if let Some(path) = misplaced_secret(&ctx.config) {
        return Err(fail(
            SECRET_MISPLACED_CLASS,
            format!("`{SECRET_REF_KEY}` is only valid in `env`; found one at `{path}`"),
        ));
    }

    let config: ProcessConfig = serde_json::from_value(ctx.config.clone()).map_err(|e| {
        fail(
            BAD_CONFIG_CLASS,
            format!("process step config is invalid: {e}"),
        )
    })?;

    // Secrets are resolved here, into the child's environment, and nowhere else.
    let mut env: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
    for (key, value) in &config.env {
        let resolved = match value {
            ValueOrSecretRef::Secret { name } => ctx
                .secrets
                .resolve(name)
                .map_err(|e| fail(SECRET_UNAVAILABLE_CLASS, e.to_string()))?,
            ValueOrSecretRef::Literal(literal) => SmolStr::new(stringify(literal)),
        };
        env.insert(key.clone(), resolved);
    }

    // The outputs file lives on the workspace, so both host and container see it.
    let output_rel = format!(".ci/out/{}.env", ctx.firing.raw());
    let output_host = ctx.env.workspace().join(&output_rel);
    if let Some(parent) = output_host.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            fail(
                WORKSPACE_CLASS,
                format!("could not create the outputs directory: {e}"),
            )
        })?;
    }
    let _ = tokio::fs::write(&output_host, "").await;
    let output_path = SmolStr::new(format!("{}/{output_rel}", ctx.env.workspace_in_env()));
    env.insert(SmolStr::new(OUTPUT_ENV), output_path.clone());
    for alias in &config.output_env_aliases {
        env.insert(alias.clone(), output_path.clone());
    }

    let (program, mut args) = config.shell.invocation();
    let mut argv: Vec<SmolStr> = args.drain(..).map(SmolStr::new).collect();
    argv.push(SmolStr::new(&config.run));
    let spec = ProcessSpec {
        program: SmolStr::new(program),
        args: argv,
        env,
        cwd: config.working_dir.clone(),
    };

    let mut handle = ctx
        .env
        .spawn(spec)
        .await
        .map_err(|e| fail(SPAWN_CLASS, e.to_string()))?;

    // Log capture runs on its own task and keeps going through cancellation, so a
    // cancelled step's final output is not lost.
    let drain = handle
        .lines()
        .map(|lines| tokio::spawn(forward_lines(lines, ctx.logs.clone())));

    let grace = ctx.env.grace();
    let ending = ladder(&mut *handle, &mut ctx, grace).await;

    if let Some(drain) = drain {
        let _ = tokio::time::timeout(DRAIN_LIMIT, drain).await;
    }

    let mut output = match read_outputs(&output_host).await {
        Ok(output) => output,
        Err(message) => return Err(fail(BAD_OUTPUT_CLASS, message)),
    };

    Ok(match ending {
        Ending::Natural(status) => {
            output.insert("exit_status".into(), exit_value(&status));
            natural_outcome(&status, &config.soft_fail, Value::Object(output))
        }
        Ending::Signalled {
            escalation,
            status: exit,
        } => {
            if let Some(exit) = &exit {
                output.insert("exit_status".into(), exit_value(exit));
            }
            // The escalation goes in the output: `Status::Cancelled` carries no
            // FailureInfo, and inventing one would mean widening a closed enum.
            output.insert(
                "cancel_escalation".into(),
                Value::String(escalation.to_string()),
            );
            Outcome::new(Status::Cancelled, Value::Object(output))
        }
    })
}

enum Ending {
    Natural(ExitStatus),
    Signalled {
        escalation: &'static str,
        status: Option<ExitStatus>,
    },
}

/// SIGTERM to the group, grace, then SIGKILL to the group.
///
/// Idempotent by construction: once the ladder has started, further `Cancel`s are
/// drained and ignored rather than restarting it.
async fn ladder(
    handle: &mut dyn executor::ProcessHandle,
    ctx: &mut StepCtx,
    grace: Duration,
) -> Ending {
    // First terminal wins. If the process exits before any signal lands, the outcome
    // is the natural one and the cancel is a no-op.
    let natural = tokio::select! {
        result = handle.wait() => Some(result),
        _ = ctx.control.recv() => None,
    };
    if let Some(result) = natural {
        return match result {
            Ok(status) => Ending::Natural(status),
            Err(_) => Ending::Natural(ExitStatus::code(-1)),
        };
    }

    let _ = handle.signal(Sig::Term).await;
    match tokio::time::timeout(grace, handle.wait()).await {
        Ok(result) => Ending::Signalled {
            escalation: "sigterm",
            status: result.ok(),
        },
        Err(_) => {
            let _ = handle.signal(Sig::Kill).await;
            let status = handle.wait().await.ok();
            Ending::Signalled {
                escalation: "sigkill",
                status,
            }
        }
    }
}

fn natural_outcome(status: &ExitStatus, soft_fail: &SoftFail, output: Value) -> Outcome {
    if status.success() {
        return Outcome::new(Status::Success, output);
    }
    if let Some(signal) = status.signal {
        // Killed by something that was not us.
        return Outcome::new(
            Status::Failure(
                FailureInfo::new(format!("step was killed by signal {signal}"))
                    .with_class(&format!("signal:{signal}")),
            ),
            output,
        );
    }
    let code = status.code.unwrap_or(-1);
    let failure = FailureInfo::exit_status(code);
    if soft_fail.matches(code) {
        // The real failure rides along in `underlying`: the log never records a clean
        // success for something that failed.
        Outcome::new(Status::partial(failure), output)
    } else {
        Outcome::new(Status::Failure(failure), output)
    }
}

async fn forward_lines(mut lines: executor::LineStream, out: mpsc::Sender<StepEvent>) {
    while let Some(LogLine { stream, line, .. }) = lines.recv().await {
        if out.send(StepEvent::Log { stream, line }).await.is_err() {
            return;
        }
    }
}

async fn read_outputs(path: &std::path::Path) -> Result<Map<String, Value>, String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => parse(&text).map_err(|e| e.to_string()),
        // No file is not an error: a step need not write outputs.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("could not read the outputs file: {e}")),
    }
}

fn exit_value(status: &ExitStatus) -> Value {
    match (status.code, status.signal) {
        (Some(code), _) => Value::from(code),
        (None, Some(signal)) => Value::String(format!("signal:{signal}")),
        _ => Value::Null,
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn fail(class: &'static str, message: impl Into<String>) -> StepFailure {
    StepFailure {
        class,
        message: message.into(),
    }
}

/// Find a `$secret` reference outside `env`, and say where it is.
fn misplaced_secret(config: &Value) -> Option<String> {
    fn walk(value: &Value, path: &str, inside_env: bool) -> Option<String> {
        match value {
            Value::Object(map) => {
                if map.contains_key(SECRET_REF_KEY) && !inside_env {
                    return Some(if path.is_empty() {
                        "<root>".into()
                    } else {
                        path.into()
                    });
                }
                map.iter().find_map(|(key, child)| {
                    let next = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    // Only the top-level `env` map is a secret-shaped position.
                    let child_in_env = path.is_empty() && key == "env";
                    walk(child, &next, child_in_env || (inside_env && path == "env"))
                })
            }
            Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(i, child)| walk(child, &format!("{path}[{i}]"), false)),
            _ => None,
        }
    }
    walk(config, "", false)
}
