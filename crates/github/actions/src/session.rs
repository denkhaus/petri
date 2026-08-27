//! The runner contract around one step.
//!
//! GitHub gives every step five files to write commands into — `GITHUB_OUTPUT`,
//! `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STATE`, `GITHUB_STEP_SUMMARY` — and applies
//! them when the step ends: outputs to the step's record, env and path to every
//! later step of the job, state to the action's later phases. A [`Session`] creates
//! the files before the step, hands the process step a config pointing at them, and
//! reads them back afterwards.
//!
//! Everything lives in the workspace, reached through `ExecEnv`, so this works the
//! same whether the job runs on this machine or in a container:
//!
//! ```text
//! <workspace>/repo/                 GITHUB_WORKSPACE — what `actions/checkout` fills
//! <workspace>/.ci/github/           the runner's own files, safe from a checkout
//!     event.json                    GITHUB_EVENT_PATH
//!     job-env.json, job-path.json   what GITHUB_ENV / GITHUB_PATH accumulated so far
//!     steps/<firing>/{env,path,state,summary}
//! <workspace>/.ci/temp, toolcache   RUNNER_TEMP, RUNNER_TOOL_CACHE
//! <workspace>/.ci/actions/…         staged action trees
//! ```
//!
//! `GITHUB_OUTPUT` is the process step's own outputs file, under its alias.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use executor::{ExecEnv, SecretProvider};
use frontend_gha::exprs::{has_secret_sentinel, replace_secret_sentinels};
use ir::{LogStream, Outcome, StepEvent, Value};
use serde_json::{Map, json};
use smol_str::SmolStr;
use steps::{ProcessConfig, ProcessStep, Step, StepCtx, StepFailure, ValueOrSecretRef};
use tokio::sync::mpsc;

use crate::commands::{CommandEffects, CommandSink};

/// The runner's own directory, relative to the workspace root.
pub const RUNNER_DIR: &str = ".ci/github";
/// `GITHUB_WORKSPACE`, relative to the workspace root.
pub const REPO_DIR: &str = "repo";
/// The step could not set up or read back its runner files.
pub const RUNNER_FILES_CLASS: &str = "runner_files";

const JOB_ENV_FILE: &str = ".ci/github/job-env.json";
const JOB_PATH_FILE: &str = ".ci/github/job-path.json";
const EVENT_FILE: &str = ".ci/github/event.json";
const TEMP_DIR: &str = ".ci/temp";
const TOOL_CACHE_DIR: &str = ".ci/toolcache";

/// How long to wait for the command sink after the process step returns. The
/// process step itself stops draining output after its own limit, so a straggler
/// holding stdout open cannot wedge the step here either.
const SINK_LIMIT: Duration = Duration::from_secs(6);

/// The step's own files, relative to the workspace root.
struct StepFiles {
    env: PathBuf,
    path: PathBuf,
    state: PathBuf,
    summary: PathBuf,
}

pub struct Session {
    env: Arc<dyn ExecEnv>,
    /// The workspace root as the process sees it.
    workspace: String,
    files: StepFiles,
    job_env: BTreeMap<String, String>,
    job_path: Vec<String>,
}

/// What the step left behind, for the step kind to fold into its outcome.
#[derive(Debug, Default)]
pub struct Effects {
    /// `::set-output::` values, for the record's output.
    pub outputs: Map<String, Value>,
    /// `GITHUB_STATE` plus `::save-state::`.
    pub state: Map<String, Value>,
    /// `GITHUB_STEP_SUMMARY`, when the step wrote one.
    pub summary: String,
}

impl Session {
    /// Create the step's files and read what the job has accumulated so far.
    pub async fn begin(ctx: &StepCtx, event: &Value) -> Result<Self, StepFailure> {
        let env = Arc::clone(&ctx.env);
        let workspace = env.workspace_path().to_string();
        let dir = PathBuf::from(RUNNER_DIR)
            .join("steps")
            .join(ctx.firing.raw().to_string());
        let files = StepFiles {
            env: dir.join("env"),
            path: dir.join("path"),
            state: dir.join("state"),
            summary: dir.join("summary"),
        };
        for file in [&files.env, &files.path, &files.state, &files.summary] {
            write(&*env, file, b"").await?;
        }
        let event = match event {
            Value::Null => json!({}),
            other => other.clone(),
        };
        let event_bytes = serde_json::to_vec(&event).unwrap_or_else(|_| b"{}".to_vec());
        write(&*env, Path::new(EVENT_FILE), &event_bytes).await?;
        for dir in [TEMP_DIR, TOOL_CACHE_DIR] {
            let keep = Path::new(dir).join(".keep");
            if read(&*env, &keep).await?.is_none() {
                write(&*env, &keep, b"").await?;
            }
        }
        let job_env: BTreeMap<String, String> = read_json(&*env, Path::new(JOB_ENV_FILE))
            .await?
            .unwrap_or_default();
        let job_path: Vec<String> = read_json(&*env, Path::new(JOB_PATH_FILE))
            .await?
            .unwrap_or_default();
        Ok(Self {
            env,
            workspace,
            files,
            job_env,
            job_path,
        })
    }

    /// The workspace root as the process sees it.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// `GITHUB_WORKSPACE`.
    pub fn github_workspace(&self) -> String {
        format!("{}/{REPO_DIR}", self.workspace)
    }

    fn absolute(&self, relative: &Path) -> String {
        format!("{}/{}", self.workspace, relative.display())
    }

    /// The environment every step gets on top of the scope's: the job's
    /// accumulated `GITHUB_ENV` first (a step's own `env:` wins over it), then the
    /// files and directories of the contract.
    pub fn env(&self, node: &str) -> BTreeMap<SmolStr, SmolStr> {
        let mut out: BTreeMap<SmolStr, SmolStr> = self
            .job_env
            .iter()
            .map(|(k, v)| (SmolStr::new(k), SmolStr::new(v)))
            .collect();
        let mut set = |key: &str, value: String| {
            out.insert(SmolStr::new(key), SmolStr::new(value));
        };
        set("GITHUB_ENV", self.absolute(&self.files.env));
        set("GITHUB_PATH", self.absolute(&self.files.path));
        set("GITHUB_STATE", self.absolute(&self.files.state));
        set("GITHUB_STEP_SUMMARY", self.absolute(&self.files.summary));
        set("GITHUB_EVENT_PATH", self.absolute(Path::new(EVENT_FILE)));
        set("GITHUB_WORKSPACE", self.github_workspace());
        set("GITHUB_ACTION", node.to_string());
        set("RUNNER_TEMP", self.absolute(Path::new(TEMP_DIR)));
        set(
            "RUNNER_TOOL_CACHE",
            self.absolute(Path::new(TOOL_CACHE_DIR)),
        );
        out
    }

    /// A shell prologue putting the job's `GITHUB_PATH` entries in front of `PATH`,
    /// newest first as GitHub does. Empty when there are none.
    pub fn prologue(&self) -> String {
        if self.job_path.is_empty() {
            return String::new();
        }
        let joined = self
            .job_path
            .iter()
            .map(|p| shell_quote(p))
            .collect::<Vec<_>>()
            .join(":");
        format!("export PATH={joined}:\"$PATH\"\n")
    }

    /// Run `process` under this session: the process step does the work, the
    /// command sink watches its output, and the files are applied afterwards.
    pub async fn run(
        mut self,
        process: ProcessConfig,
        ctx: StepCtx,
        allow_unsecure: bool,
    ) -> (Outcome, Effects) {
        let StepCtx {
            firing,
            attempt,
            node,
            config: _,
            env,
            secrets,
            caps,
            logs,
            control,
        } = ctx;
        let process = match resolve_secret_sentinels(process, secrets.as_ref()) {
            Ok(process) => process,
            Err(failure) => return (failure.into(), Effects::default()),
        };
        let (tx, rx) = mpsc::channel(64);
        let sink = CommandSink::new(logs.clone(), secrets.masker(), allow_unsecure);
        let collected = sink.effects();
        let sink_task = tokio::spawn(sink.run(rx));
        let delegate = StepCtx {
            firing,
            attempt,
            node,
            config: Value::Null,
            env,
            secrets,
            caps,
            logs: tx,
            control,
        };
        let outcome = Step::run(&ProcessStep, process, delegate).await;
        let _ = tokio::time::timeout(SINK_LIMIT, sink_task).await;
        let commands = std::mem::take(&mut *collected.lock().expect("effects are not poisoned"));

        let effects = match self.finish(commands).await {
            Ok(effects) => effects,
            Err(failure) => {
                let _ = logs
                    .send(StepEvent::Log {
                        stream: LogStream::Stderr,
                        line: format!("Warning: {}", failure.message),
                    })
                    .await;
                Effects::default()
            }
        };
        if !effects.summary.is_empty() {
            let _ = logs
                .send(StepEvent::Custom(
                    json!({ "github/step_summary": effects.summary }),
                ))
                .await;
        }
        (outcome, effects)
    }

    /// Read the files back and apply them: env and path to the job, state and
    /// outputs to the caller.
    async fn finish(&mut self, commands: CommandEffects) -> Result<Effects, StepFailure> {
        let env_text = read_text(&*self.env, &self.files.env).await?;
        let state_text = read_text(&*self.env, &self.files.state).await?;
        let path_text = read_text(&*self.env, &self.files.path).await?;
        let summary = read_text(&*self.env, &self.files.summary).await?;

        let mut env = parse_env_file(&env_text, "GITHUB_ENV")?;
        env.extend(commands.env);
        for (key, value) in env {
            self.job_env.insert(key, stringify(&value));
        }
        // Each entry goes in front of the ones before it.
        let new_paths = path_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .chain(commands.path);
        for path in new_paths {
            self.job_path.retain(|p| p != &path);
            self.job_path.insert(0, path);
        }
        write(
            &*self.env,
            Path::new(JOB_ENV_FILE),
            &serde_json::to_vec(&self.job_env).expect("a string map encodes"),
        )
        .await?;
        write(
            &*self.env,
            Path::new(JOB_PATH_FILE),
            &serde_json::to_vec(&self.job_path).expect("a string list encodes"),
        )
        .await?;

        let mut state = parse_env_file(&state_text, "GITHUB_STATE")?;
        state.extend(commands.state);
        Ok(Effects {
            outputs: commands.outputs,
            state,
            summary,
        })
    }
}

/// Replace the secret sentinels the frontend lowered into the script and the env
/// with their values, from the run's provider — which registers each for masking
/// as it resolves it. A whole-value `$secret` reference is left for the process
/// step, which resolves it the same way.
fn resolve_secret_sentinels(
    mut process: ProcessConfig,
    secrets: &dyn SecretProvider,
) -> Result<ProcessConfig, StepFailure> {
    let mut resolve = |name: &str| -> Result<String, StepFailure> {
        secrets
            .resolve(name)
            .map(|secret| secret.expose().to_string())
            .map_err(|e| StepFailure {
                class: steps::SECRET_UNAVAILABLE_CLASS,
                message: e.to_string(),
            })
    };
    if has_secret_sentinel(&process.run) {
        process.run = replace_secret_sentinels(&process.run, &mut resolve)?;
    }
    for value in process.env.values_mut() {
        if let ValueOrSecretRef::Literal(Value::String(text)) = value
            && has_secret_sentinel(text)
        {
            *text = replace_secret_sentinels(text, &mut resolve)?;
        }
    }
    Ok(process)
}

/// `key=value` lines and `key<<DELIM … DELIM` blocks: the format shared by
/// `GITHUB_OUTPUT`, `GITHUB_ENV` and `GITHUB_STATE`.
fn parse_env_file(text: &str, what: &str) -> Result<Map<String, Value>, StepFailure> {
    steps::parse_outputs(text).map_err(|e| StepFailure {
        class: RUNNER_FILES_CLASS,
        message: format!("could not read what the step wrote to `{what}`: {e}"),
    })
}

/// Fold what a step left into its outcome: `set-output` values that the outputs
/// file did not already provide, and the state for the action's later phases.
pub fn fold_into_outcome(
    mut outcome: Outcome,
    effects: Effects,
    state: Map<String, Value>,
) -> Outcome {
    if !outcome.output.is_object() {
        outcome.output = Value::Object(Map::new());
    }
    let output = outcome.output.as_object_mut().expect("just made an object");
    for (key, value) in effects.outputs {
        output.entry(key).or_insert(value);
    }
    if !state.is_empty() {
        output.insert(
            frontend_gha::STATE_OUTPUT_KEY.to_string(),
            Value::Object(state),
        );
    }
    outcome
}

/// Whether the resolved env sets a variable to a GitHub-truthy value.
pub fn env_truthy(env: &BTreeMap<SmolStr, ValueOrSecretRef>, key: &str) -> bool {
    match env.get(key) {
        Some(ValueOrSecretRef::Literal(value)) => {
            let s = stringify(value);
            !s.is_empty() && s != "false" && s != "0"
        }
        _ => false,
    }
}

/// A JSON value as the string a process sees, the way the process step does it.
pub fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Single-quote `s` for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn write(env: &dyn ExecEnv, relative: &Path, contents: &[u8]) -> Result<(), StepFailure> {
    env.write_file(relative, contents)
        .await
        .map_err(|e| StepFailure {
            class: RUNNER_FILES_CLASS,
            message: format!("could not write `{}`: {e}", relative.display()),
        })
}

async fn read(env: &dyn ExecEnv, relative: &Path) -> Result<Option<Vec<u8>>, StepFailure> {
    env.read_file(relative).await.map_err(|e| StepFailure {
        class: RUNNER_FILES_CLASS,
        message: format!("could not read `{}`: {e}", relative.display()),
    })
}

async fn read_text(env: &dyn ExecEnv, relative: &Path) -> Result<String, StepFailure> {
    Ok(read(env, relative)
        .await?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default())
}

async fn read_json<T: serde::de::DeserializeOwned>(
    env: &dyn ExecEnv,
    relative: &Path,
) -> Result<Option<T>, StepFailure> {
    let Some(bytes) = read(env, relative).await? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| StepFailure {
            class: RUNNER_FILES_CLASS,
            message: format!(
                "`{}` is not what this runner wrote: {e}",
                relative.display()
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_truthiness() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        let mut env = BTreeMap::new();
        env.insert(
            SmolStr::new("A"),
            ValueOrSecretRef::Literal(Value::String("true".into())),
        );
        env.insert(
            SmolStr::new("B"),
            ValueOrSecretRef::Literal(Value::String("false".into())),
        );
        assert!(env_truthy(&env, "A"));
        assert!(!env_truthy(&env, "B"));
        assert!(!env_truthy(&env, "C"));
    }
}
