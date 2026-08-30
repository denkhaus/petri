//! The runner contract around one step.
//!
//! GitHub gives every step five files to write commands into — `GITHUB_OUTPUT`,
//! `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STATE`, `GITHUB_STEP_SUMMARY` — and
//! applies them when the step ends: outputs to the step's record, env and path
//! to every later step of the job, state to the action's later phases. A
//! [`Session`] creates the files before the step, hands the process step a
//! config pointing at them, and reads them back afterwards.
//!
//! Everything lives in the workspace, reached through `ExecEnv`, so this works
//! the same whether the job runs on this machine or in a container:
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
use std::convert::Infallible;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{borrow, mem};

use executor::{ExecEnv, SecretProvider};
use frontend_gha::exprs::{
    escape_sentinel_text, has_env_sentinel, has_runner_temp_sentinel,
    has_runner_tool_cache_sentinel, has_secret_sentinel, has_sentinel_escape,
    has_workspace_sentinel, replace_env_sentinels, replace_runner_temp_sentinels,
    replace_runner_tool_cache_sentinels, replace_secret_sentinels, replace_workspace_sentinels,
    secret_sentinel, unescape_sentinel_text,
};
use ir::{LogStream, Outcome, StepEvent, Value};
use serde::de::DeserializeOwned;
use serde_json::{Map, json};
use smol_str::SmolStr;
use steps::{ProcessConfig, ProcessStep, Shell, Step, StepCtx, StepFailure, ValueOrSecretRef};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;

use crate::commands::{CommandEffects, CommandSink};
use crate::config::try_map_process_texts;
use crate::hashfiles;

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
/// GitHub command files are control input, not general artifact storage.
const COMMAND_FILE_LIMIT: usize = 1024 * 1024;

/// How long to wait for the command sink after the process step returns. The
/// process step itself stops draining output after its own limit, so a
/// straggler holding stdout open cannot wedge the step here either.
pub(crate) const SINK_LIMIT: Duration = Duration::from_secs(6);

/// The step's own files, relative to the workspace root.
struct StepFiles {
    env:     PathBuf,
    path:    PathBuf,
    state:   PathBuf,
    summary: PathBuf,
}

pub struct Session {
    env:        Arc<dyn ExecEnv>,
    /// The workspace root as the process sees it.
    workspace:  String,
    files:      StepFiles,
    job_env:    BTreeMap<String, String>,
    job_path:   Vec<String>,
    /// The host's persistent tool cache, when the host registered one
    /// ([`crate::ToolCacheCap`]): [`resolved_tool_cache`] points steps at it
    /// where this environment's filesystem has it.
    tool_cache: Option<PathBuf>,
}

/// What the step left behind, for the step kind to fold into its outcome.
#[derive(Debug, Default)]
pub struct Effects {
    /// `::set-output::` values, for the record's output.
    pub outputs: Map<String, Value>,
    /// `GITHUB_STATE` plus `::save-state::`.
    pub state:   Map<String, Value>,
    /// `GITHUB_STEP_SUMMARY`, when the step wrote one.
    pub summary: String,
    /// Commands the sink refused (`set-env`/`add-path` without the opt-in):
    /// each fails the step, as the runner's `CommandResult` does.
    pub refused: Vec<String>,
}

/// The job's accumulated `GITHUB_ENV`, read without creating any session files:
/// what a gate's `env.NAME` leaf sees before the step commits to running.
pub(crate) async fn read_job_env(
    env: &dyn ExecEnv,
) -> Result<BTreeMap<String, String>, StepFailure> {
    Ok(read_json(env, Path::new(JOB_ENV_FILE))
        .await?
        .unwrap_or_default())
}

/// `GITHUB_WORKSPACE` as the job environment sees it, without a session.
pub(crate) fn github_workspace_path(env: &dyn ExecEnv) -> String {
    format!("{}/{REPO_DIR}", env.workspace_path())
}

/// The per-run workspace tool cache under `root` — the weakest default of
/// [`resolved_tool_cache`].
pub(crate) fn workspace_tool_cache(root: &str) -> String {
    format!("{root}/{TOOL_CACHE_DIR}")
}

/// `RUNNER_TOOL_CACHE` as a process rooted at `root` will see it: the one
/// computation behind the exported variable, the `runner.tool_cache` sentinel
/// substitution, and the gate evaluator, so the expression and the environment
/// cannot diverge.
///
/// The resolution mirrors the retired shell prologue's, weakest default last:
/// the step's own `env:` (`env_config`), then the job's accumulated
/// `GITHUB_ENV` (`job_env`), then the environment's own variable (`ambient` —
/// a runner image ships a populated `/opt/hostedtoolcache` and says so in its
/// env), then the host's persistent store (`store`, already gated on this
/// filesystem having it), then the per-run workspace directory. Empty values
/// fall through, as the prologue's `-z` did. GitHub's `runner.tool_cache` is a
/// constant; that the first two rungs can move it mid-job is this runner's
/// documented divergence, kept because the exported variable must match.
pub(crate) fn resolved_tool_cache(
    root: &str,
    ambient: Option<String>,
    store: Option<&Path>,
    env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
    job_env: &BTreeMap<String, String>,
) -> String {
    // A configured value may itself carry runner-side path sentinels; resolve
    // them against the same root so every consumer sees the exec-time text. A
    // value still carrying a sentinel after that (a secret, a hash) cannot be
    // known here and falls through.
    let resolved = |text: &str| {
        let mut text = text.to_string();
        if has_workspace_sentinel(&text) {
            text = replace_workspace_sentinels(&text, &format!("{root}/{REPO_DIR}"));
        }
        if has_runner_temp_sentinel(&text) {
            text = replace_runner_temp_sentinels(&text, &runner_temp_path(root));
        }
        (!text.is_empty() && !text.contains('\u{E000}')).then_some(text)
    };
    let configured = match env_config.get("RUNNER_TOOL_CACHE") {
        Some(ValueOrSecretRef::Literal(value)) => resolved(&stringify(value)),
        _ => None,
    };
    configured
        .or_else(|| job_env.get("RUNNER_TOOL_CACHE").and_then(|v| resolved(v)))
        .or_else(|| ambient.filter(|v| !v.is_empty()))
        .or_else(|| store.map(|s| s.display().to_string()))
        .unwrap_or_else(|| workspace_tool_cache(root))
}

/// [`resolved_tool_cache`] for a process running in `env` itself — a shell or
/// node step: the ambient env is the executor's fact, and the host store
/// counts only where this environment's filesystem has it (petri mounts
/// nothing but the workspace into containers, so a containerized job never
/// sees host paths).
pub(crate) fn env_tool_cache(
    env: &dyn ExecEnv,
    store: Option<&Path>,
    env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
    job_env: &BTreeMap<String, String>,
) -> String {
    resolved_tool_cache(
        env.workspace_path(),
        env.ambient_env("RUNNER_TOOL_CACHE"),
        if env.shares_host_filesystem() {
            store
        } else {
            None
        },
        env_config,
        job_env,
    )
}

/// `RUNNER_TEMP` under `root`: the one computation behind both the exported
/// variable ([`Session::env_rooted`]) and the `runner.temp` sentinel
/// substitution, so the expression and the environment cannot diverge.
pub(crate) fn runner_temp_path(root: &str) -> String {
    format!("{root}/{TEMP_DIR}")
}

impl Session {
    /// Create the step's files and read what the job has accumulated so far.
    pub async fn begin(ctx: &StepCtx, event: &Value) -> Result<Self, StepFailure> {
        let env = ctx.env.clone();
        let workspace = env.workspace_path().to_string();
        let dir = PathBuf::from(RUNNER_DIR)
            .join("steps")
            .join(ctx.firing.raw().to_string());
        let files = StepFiles {
            env:     dir.join("env"),
            path:    dir.join("path"),
            state:   dir.join("state"),
            summary: dir.join("summary"),
        };
        tokio::try_join!(
            write(&*env, &files.env, b""),
            write(&*env, &files.path, b""),
            write(&*env, &files.state, b""),
            write(&*env, &files.summary, b""),
        )?;
        let event = match event {
            Value::Null => json!({}),
            other => other.clone(),
        };
        let event_bytes = serde_json::to_vec(&event).unwrap_or_else(|_| b"{}".to_vec());
        let temp_keep = Path::new(TEMP_DIR).join(".keep");
        let tool_keep = Path::new(TOOL_CACHE_DIR).join(".keep");
        let ((), (), (), job_env, job_path) = tokio::try_join!(
            write(&*env, Path::new(EVENT_FILE), &event_bytes),
            write(&*env, &temp_keep, b""),
            write(&*env, &tool_keep, b""),
            read_json(&*env, Path::new(JOB_ENV_FILE)),
            read_json(&*env, Path::new(JOB_PATH_FILE)),
        )?;
        let job_env: BTreeMap<String, String> = job_env.unwrap_or_default();
        let job_path: Vec<String> = job_path.unwrap_or_default();
        let tool_cache = ctx
            .capability::<crate::ToolCacheCap>()
            .map(|cap| cap.0.clone());
        Ok(Self {
            env,
            workspace,
            files,
            job_env,
            job_path,
            tool_cache,
        })
    }

    /// The workspace root as the process sees it.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// The checkout root — `GITHUB_WORKSPACE`, which is [`Self::workspace`]
    /// plus the repository directory, not the workspace root itself.
    pub fn github_workspace(&self) -> String {
        github_workspace_path(&*self.env)
    }

    /// [`resolved_tool_cache`] for a one-shot action container mounted at
    /// `root`: the explicit export overrides the action image's own env and
    /// no host path reaches inside, so neither ambient env nor the store
    /// applies — the chain is the step's `env:`, the job's, the workspace.
    pub fn container_tool_cache(
        &self,
        env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
        root: &str,
    ) -> String {
        resolved_tool_cache(root, None, None, env_config, &self.job_env)
    }

    /// The environment every step gets on top of the scope's: the job's
    /// accumulated `GITHUB_ENV` first (a step's own `env:` wins over it), then
    /// the files and directories of the contract.
    ///
    /// `RUNNER_TOOL_CACHE` is deliberately *not* here: its value depends on
    /// the step's own `env:` config ([`resolved_tool_cache`]), so
    /// [`Session::run`] exports it once the config is assembled, and a
    /// one-shot action container gets [`Session::container_tool_cache`].
    pub fn env(&self, node: &str) -> BTreeMap<SmolStr, SmolStr> {
        self.env_rooted(node, &self.workspace)
    }

    /// [`Session::env`], with every path under `root` instead of this
    /// environment's workspace path: what a process sees when the workspace is
    /// mounted somewhere else — a one-shot action container's mount point.
    pub fn env_rooted(&self, node: &str, root: &str) -> BTreeMap<SmolStr, SmolStr> {
        let mut out: BTreeMap<SmolStr, SmolStr> = self
            .job_env
            .iter()
            .map(|(k, v)| (SmolStr::new(k), SmolStr::new(v)))
            .collect();
        let rooted = |relative: &Path| format!("{root}/{}", relative.display());
        let mut set = |key: &str, value: String| {
            out.insert(SmolStr::new(key), SmolStr::new(value));
        };
        set("GITHUB_ENV", rooted(&self.files.env));
        set("GITHUB_PATH", rooted(&self.files.path));
        set("GITHUB_STATE", rooted(&self.files.state));
        set("GITHUB_STEP_SUMMARY", rooted(&self.files.summary));
        set("GITHUB_EVENT_PATH", rooted(Path::new(EVENT_FILE)));
        set("GITHUB_WORKSPACE", format!("{root}/{REPO_DIR}"));
        set("GITHUB_ACTION", node.to_string());
        set("RUNNER_TEMP", runner_temp_path(root));
        out
    }

    /// The job's accumulated `GITHUB_ENV`, for a step kind that assembles its
    /// process environment itself (the docker action step).
    pub(crate) fn job_env(&self) -> &BTreeMap<String, String> {
        &self.job_env
    }

    /// Replace the `env.NAME` sentinels — bare `${{ env.NAME }}` references in
    /// step config — with the value the name has in the environment this
    /// process receives: the resolved env config first (the step's own `env:`,
    /// with the job's accumulated `GITHUB_ENV` and the contract's variables
    /// already merged beneath it), then the ambient environment. One truth for
    /// the expression and the variable — the `runner.temp` invariant — so a
    /// `GITHUB_ENV` append from an earlier step is visible here, as it is on
    /// GitHub and in this runner's gates.
    ///
    /// Env values resolve first, from the job's accumulated file and the
    /// ambient env alone — a step's `env:` block is not in its own scope, so
    /// nothing can chase its own tail — and the run text after them, so a
    /// value spliced from the config carries no marker of its own. A secret
    /// reference splices its sentinel for the secret pass right after this one
    /// to resolve (and register for masking); runtime text — the env file, the
    /// ambient env — splices escaped, so nothing a step exported can
    /// impersonate a marker.
    fn resolve_env_sentinels(&self, mut process: ProcessConfig) -> ProcessConfig {
        let runtime = |name: &str| {
            ci_get(&self.job_env, name)
                .cloned()
                .or_else(|| self.env.ambient_env(name))
                .map(|value| escape_sentinel_text(&value))
                .unwrap_or_default()
        };
        for value in process.env.values_mut() {
            if let ValueOrSecretRef::Literal(Value::String(text)) = value
                && has_env_sentinel(text)
            {
                *text = replace_env_sentinels(text, |name| -> Result<String, Infallible> {
                    Ok(runtime(name))
                })
                .expect("the runtime lookup is infallible");
            }
        }
        if has_env_sentinel(&process.run) {
            let env = &process.env;
            process.run =
                replace_env_sentinels(&process.run, |name| -> Result<String, Infallible> {
                    Ok(match ci_get(env, name) {
                        Some(ValueOrSecretRef::Literal(v)) => stringify(v),
                        Some(ValueOrSecretRef::Secret { name }) => secret_sentinel(name),
                        None => self
                            .env
                            .ambient_env(name)
                            .map(|value| escape_sentinel_text(&value))
                            .unwrap_or_default(),
                    })
                })
                .expect("the config lookup is infallible");
        }
        process
    }

    /// A shell prologue: the job's `GITHUB_PATH` entries in front of `PATH`,
    /// newest first as GitHub does, and the runner image's Docker daemon
    /// started once. Plain POSIX, like the rest of the runner scripts.
    ///
    /// The tool cache once resolved here, at exec time; it is now computed in
    /// Rust ([`resolved_tool_cache`]) and exported by [`Session::run`] with
    /// the rest of the environment, where the `runner.tool_cache` sentinel
    /// substitution and the gate evaluator share it.
    pub fn prologue(&self) -> String {
        let mut out = String::new();
        if !self.job_path.is_empty() {
            let joined = self
                .job_path
                .iter()
                .map(|p| shell_quote(p))
                .collect::<Vec<_>>()
                .join(":");
            let _ = writeln!(out, "export PATH={joined}:\"$PATH\"");
        }
        // A runner image that ships a Docker engine leaves its daemon stopped
        // (containers get no init to supervise one) and provides a
        // `start-docker` helper. The first step brings the daemon up; the
        // marker keeps every later prologue free. Images without the helper
        // skip the whole block, so this costs a `command -v` everywhere else.
        out.push_str(
            "if [ ! -e /tmp/.petri-dockerd ] && command -v start-docker >/dev/null 2>&1; then \
             : > /tmp/.petri-dockerd; start-docker >/dev/null 2>&1 || true; fi\n",
        );
        out
    }

    /// Write the resolved script to the step's `script` file and turn the
    /// process into `sh` running the shell template over it.
    async fn stage_script(
        &self,
        mut process: ProcessConfig,
        template: &str,
    ) -> Result<ProcessConfig, StepFailure> {
        let step_id = self
            .files
            .env
            .parent()
            .and_then(Path::file_name)
            .expect("step files have a firing directory");
        let script_arg = PathBuf::from(".petri").join(step_id).join("script");
        let script = process
            .working_dir
            .as_deref()
            .unwrap_or_else(|| Path::new(""))
            .join(&script_arg);
        write(&*self.env, &script, process.run.as_bytes()).await?;
        process.run = format!(
            "{}exec {}\n",
            self.prologue(),
            template.replace("{0}", &script_arg.to_string_lossy())
        );
        process.shell = Shell::Sh;
        Ok(process)
    }

    /// Run `process` under this session: the process step does the work, the
    /// command sink watches its output, and the files are applied afterwards.
    ///
    /// With a `shell_command` template, `process.run` is the bare script: after
    /// the sentinels resolve it is written to the step's `script` file, and the
    /// process becomes `sh` running the prologue plus the template with `{0}`
    /// substituted by the script's path — as GitHub invokes custom shells.
    pub async fn run(
        mut self,
        process: ProcessConfig,
        shell_command: Option<String>,
        ctx: StepCtx,
        allow_unsecure: bool,
    ) -> (Outcome, Effects) {
        // `hashFiles` sentinels first: the hash is computed in the job
        // environment against the workspace, before any secret enters the config.
        let process = match hashfiles::resolve_hashfiles(
            process,
            &*ctx.env,
            &self.github_workspace(),
        )
        .await
        {
            Ok(process) => process,
            Err(failure) => return (failure.into(), Effects::default()),
        };
        // The tool cache, from the config as it will execute (the hashes are
        // already spliced; a path sentinel in the configured value resolves
        // inside), then substituted and exported in one breath.
        let tool_cache = env_tool_cache(
            &*self.env,
            self.tool_cache.as_deref(),
            &process.env,
            &self.job_env,
        );
        let mut process = resolve_workspace_sentinels(
            process,
            &self.github_workspace(),
            &runner_temp_path(&self.workspace),
            &tool_cache,
        );
        // The export: the one resolved value, so the variable and the
        // sentinel cannot diverge. A configured literal resolves to itself; a
        // configured secret is the step's own to keep.
        if !matches!(
            process.env.get("RUNNER_TOOL_CACHE"),
            Some(ValueOrSecretRef::Secret { .. })
        ) {
            process.env.insert(
                SmolStr::new("RUNNER_TOOL_CACHE"),
                ValueOrSecretRef::Literal(Value::String(tool_cache)),
            );
        }
        // The `env.NAME` sentinels next, from the environment now assembled —
        // before the secret pass, which resolves whatever they spliced.
        let process = self.resolve_env_sentinels(process);
        let StepCtx {
            firing,
            attempt,
            node,
            config: _,
            env,
            runner,
            secrets,
            caps,
            logs,
            control,
        } = ctx;
        let process = match resolve_secret_sentinels(process, secrets.as_ref()) {
            Ok(process) => process,
            Err(failure) => return (failure.into(), Effects::default()),
        };
        let process = match shell_command {
            Some(template) => match self.stage_script(process, &template).await {
                Ok(process) => process,
                Err(failure) => return (failure.into(), Effects::default()),
            },
            None => process,
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
            runner,
            secrets,
            caps,
            logs: tx,
            control,
        };
        let soft_fail = process.soft_fail.clone();
        let outcome = Step::run(&ProcessStep, process, delegate).await;
        let commands = settle_sink(sink_task, &collected).await;
        self.conclude(outcome, &soft_fail, commands, &logs).await
    }

    /// Finish the session and surface what it left behind: the step-summary
    /// event when the step wrote one, and — when a runner file cannot be read
    /// back — an error in the step's log and a failed step, as the runner's
    /// `FileCommandManager.ProcessFiles` does when a file command throws
    /// (`CommandResult = TaskResult.Failed`, merged into the step's result).
    /// A success-like outcome is overturned ([`runner_files_failure`]); one
    /// that already failed or was cancelled stands. A file the step deleted
    /// is not a failure on either runner: nothing was written.
    pub(crate) async fn conclude(
        &mut self,
        outcome: Outcome,
        soft_fail: &steps::SoftFail,
        commands: CommandEffects,
        logs: &mpsc::Sender<StepEvent>,
    ) -> (Outcome, Effects) {
        // A refusal outlives a read-back failure: the verdict came from the
        // command stream, not the files, and the fold reports it in its own
        // words when the outcome is still success-like.
        let refused = commands.refused.clone();
        let (outcome, effects) = match self.finish(commands).await {
            Ok(effects) => (outcome, effects),
            Err(failure) => {
                let _ = logs
                    .send(StepEvent::Log {
                        stream: LogStream::Stderr,
                        line:   format!("Error: {}", failure.message),
                    })
                    .await;
                (runner_files_failure(outcome, soft_fail, failure), Effects {
                    refused,
                    ..Effects::default()
                })
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
    pub(crate) async fn finish(
        &mut self,
        commands: CommandEffects,
    ) -> Result<Effects, StepFailure> {
        let (env_text, state_text, path_text, summary) = tokio::try_join!(
            read_text(&*self.env, &self.files.env),
            read_text(&*self.env, &self.files.state),
            read_text(&*self.env, &self.files.path),
            read_text(&*self.env, &self.files.summary),
        )?;

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
        let job_env = serde_json::to_vec(&self.job_env).expect("a string map encodes");
        let job_path = serde_json::to_vec(&self.job_path).expect("a string list encodes");
        tokio::try_join!(
            write(&*self.env, Path::new(JOB_ENV_FILE), &job_env),
            write(&*self.env, Path::new(JOB_PATH_FILE), &job_path),
        )?;

        let mut state = parse_env_file(&state_text, "GITHUB_STATE")?;
        state.extend(commands.state);
        Ok(Effects {
            outputs: commands.outputs,
            state,
            summary,
            refused: commands.refused,
        })
    }
}

/// Forward a process handle's output lines into `sink` as log events, on
/// their own task; `None` when the handle exposes no line stream. The task
/// ends with the stream — or as soon as the receiver is gone.
pub(crate) fn forward_lines(
    lines: Option<executor::LineStream>,
    sink: mpsc::Sender<StepEvent>,
) -> Option<JoinHandle<()>> {
    let mut lines = lines?;
    Some(tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if sink
                .send(StepEvent::Log {
                    stream: line.stream,
                    line:   line.line,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }))
}

/// Join the command sink and take what it collected; a sink that will not stop
/// is abandoned rather than waited on forever.
pub(crate) async fn settle_sink(
    mut sink_task: JoinHandle<()>,
    collected: &Arc<Mutex<CommandEffects>>,
) -> CommandEffects {
    if time::timeout(SINK_LIMIT, &mut sink_task).await.is_err() {
        sink_task.abort();
        let _ = sink_task.await;
    }
    mem::take(&mut *collected.lock().expect("effects are not poisoned"))
}

/// Resolve the secret sentinels in one text, from the run's provider — which
/// registers each value for masking as it resolves it — then unescape. `None`
/// means the text carries neither a sentinel nor an escape and stands as it is.
///
/// The escape-resolve-unescape ordering is the masking contract; every path
/// that turns sentinels into plaintext goes through here.
pub(crate) fn resolve_sentinel_text(
    text: &str,
    secrets: &dyn SecretProvider,
) -> Result<Option<String>, StepFailure> {
    let has_secret = has_secret_sentinel(text);
    if !has_secret && !has_sentinel_escape(text) {
        return Ok(None);
    }
    let resolved = if has_secret {
        replace_secret_sentinels(text, |name| {
            secrets
                .resolve(name)
                .map(|secret| escape_sentinel_text(&secret.expose()))
                .map_err(|e| StepFailure {
                    class:   steps::SECRET_UNAVAILABLE_CLASS,
                    message: e.to_string(),
                })
        })?
    } else {
        text.to_string()
    };
    Ok(Some(unescape_sentinel_text(&resolved)))
}

/// Replace the `github.workspace`, `runner.temp` and `runner.tool_cache`
/// sentinels in every configured text with this environment's own paths —
/// runner-side truth the lowering could not know (host paths here; a one-shot
/// container resolves against its mount).
fn resolve_workspace_sentinels(
    mut process: ProcessConfig,
    workspace: &str,
    runner_temp: &str,
    tool_cache: &str,
) -> ProcessConfig {
    let infallible: Result<(), Infallible> = try_map_process_texts(&mut process, |text| {
        let ws = has_workspace_sentinel(text);
        let temp = has_runner_temp_sentinel(text);
        let tool = has_runner_tool_cache_sentinel(text);
        if !ws && !temp && !tool {
            return Ok(None);
        }
        let mut out = text.to_string();
        if ws {
            out = replace_workspace_sentinels(&out, workspace);
        }
        if temp {
            out = replace_runner_temp_sentinels(&out, runner_temp);
        }
        if tool {
            out = replace_runner_tool_cache_sentinels(&out, tool_cache);
        }
        Ok(Some(out))
    });
    let _ = infallible;
    process
}

/// One name from an env-shaped map, exact then case-insensitive on a miss, as
/// the `env` context's lookups are.
pub(crate) fn ci_get<'m, K, V>(map: &'m BTreeMap<K, V>, name: &str) -> Option<&'m V>
where
    K: borrow::Borrow<str> + Ord,
{
    if let Some(value) = map.get(name) {
        return Some(value);
    }
    let lowered = name.to_lowercase();
    map.iter()
        .find(|(k, _)| k.borrow().to_lowercase() == lowered)
        .map(|(_, v)| v)
}

/// Replace the secret sentinels the frontend lowered into the script and the
/// env with their values. A whole-value `$secret` reference is left for the
/// process step, which resolves it the same way.
fn resolve_secret_sentinels(
    mut process: ProcessConfig,
    secrets: &dyn SecretProvider,
) -> Result<ProcessConfig, StepFailure> {
    try_map_process_texts(&mut process, |text| resolve_sentinel_text(text, secrets))?;
    Ok(process)
}

/// `key=value` lines and `key<<DELIM … DELIM` blocks: the format shared by
/// `GITHUB_OUTPUT`, `GITHUB_ENV` and `GITHUB_STATE`.
fn parse_env_file(text: &str, what: &str) -> Result<Map<String, Value>, StepFailure> {
    steps::parse_outputs(text).map_err(|e| StepFailure {
        class:   RUNNER_FILES_CLASS,
        message: format!("could not read what the step wrote to `{what}`: {e}"),
    })
}

/// A step whose runner files could not be read back, as the runner reports
/// it: the file-command failure merges into the step's result. A success-like
/// outcome becomes this failure — softened by `continue-on-error` exactly as
/// an exit status would be — while a step that already failed, or was
/// cancelled, keeps its own status. The output the process produced stays on
/// the record either way.
fn runner_files_failure(
    mut outcome: Outcome,
    soft_fail: &steps::SoftFail,
    failure: StepFailure,
) -> Outcome {
    if !outcome.status.is_success_like() {
        return outcome;
    }
    let info = ir::FailureInfo::new(failure.message).with_class(failure.class);
    outcome.status = match soft_fail {
        steps::SoftFail::All(true) => ir::Status::partial(info),
        // This failure carries no exit code, so a code list can never soften it.
        steps::SoftFail::All(false) | steps::SoftFail::ExitStatuses(_) | steps::SoftFail::Off => {
            ir::Status::Failure(info)
        }
    };
    outcome
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
    // A refused command fails the step whatever its process reported: the
    // runner's `TryProcessCommand` sets `CommandResult = Failed` when an
    // extension throws, and the step's result folds that in after the process
    // ends. A failure the process already earned stands as it is.
    if !effects.refused.is_empty() && outcome.status.is_success_like() {
        let message = format!(
            "the step's `::{}::` command was refused: unsecure commands are disabled \
             (set `ACTIONS_ALLOW_UNSECURE_COMMANDS: true` to allow them)",
            effects.refused.join("::`, `::")
        );
        outcome.status =
            ir::Status::Failure(ir::FailureInfo::new(message).with_class(COMMAND_REFUSED_CLASS));
    }
    outcome
}

/// A step whose process succeeded but whose command stream carried a refused
/// `set-env`/`add-path`.
pub const COMMAND_REFUSED_CLASS: &str = "command_refused";

/// Whether unsecure `::set-env`/`::add-path` commands are allowed, by the
/// runner's rule: `ACTIONS_ALLOW_UNSECURE_COMMANDS` parses as `true`
/// (`bool.TryParse` — `true`/`false`, any case; nothing else counts) either in
/// the step's resolved env or in the environment the process inherits — the
/// job's `env:`, or the runner's own environment — which is the runner's
/// `Environment.GetEnvironmentVariable(...) || env context` check.
pub fn can_use_unsecure_commands(
    env: &BTreeMap<SmolStr, ValueOrSecretRef>,
    exec: &dyn ExecEnv,
) -> bool {
    let step = match env.get(UNSECURE_COMMANDS_KEY) {
        Some(ValueOrSecretRef::Literal(value)) => Some(stringify(value)),
        _ => None,
    };
    unsecure_flag(step.as_deref(), exec)
}

/// [`can_use_unsecure_commands`] over an already-stringified step value.
pub(crate) fn unsecure_flag(step_value: Option<&str>, exec: &dyn ExecEnv) -> bool {
    let is_true = |s: &str| s.trim().eq_ignore_ascii_case("true");
    step_value.is_some_and(is_true)
        || exec
            .ambient_env(UNSECURE_COMMANDS_KEY)
            .as_deref()
            .is_some_and(is_true)
}

pub(crate) const UNSECURE_COMMANDS_KEY: &str = "ACTIONS_ALLOW_UNSECURE_COMMANDS";

/// Whether the resolved env sets a variable to a GitHub-truthy value.
pub fn is_env_truthy(env: &BTreeMap<SmolStr, ValueOrSecretRef>, key: &str) -> bool {
    match env.get(key) {
        Some(ValueOrSecretRef::Literal(value)) => {
            let s = stringify(value);
            !s.is_empty() && s != "false" && s != "0"
        }
        _ => false,
    }
}

/// A JSON value as the string a process sees — the process step's own rule,
/// re-exported so the two can never drift again (they once did, on null).
pub use steps::stringify;

/// Single-quote `s` for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn write(env: &dyn ExecEnv, relative: &Path, contents: &[u8]) -> Result<(), StepFailure> {
    env.write_file(relative, contents)
        .await
        .map_err(|e| StepFailure {
            class:   RUNNER_FILES_CLASS,
            message: format!("could not write `{}`: {e}", relative.display()),
        })
}

async fn read_text(env: &dyn ExecEnv, relative: &Path) -> Result<String, StepFailure> {
    Ok(read_limited(env, relative)
        .await?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default())
}

async fn read_json<T: DeserializeOwned>(
    env: &dyn ExecEnv,
    relative: &Path,
) -> Result<Option<T>, StepFailure> {
    let Some(bytes) = read_limited(env, relative).await? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| StepFailure {
            class:   RUNNER_FILES_CLASS,
            message: format!(
                "`{}` is not what this runner wrote: {e}",
                relative.display()
            ),
        })
}

async fn read_limited(env: &dyn ExecEnv, relative: &Path) -> Result<Option<Vec<u8>>, StepFailure> {
    env.read_file_limited(relative, COMMAND_FILE_LIMIT)
        .await
        .map_err(|e| StepFailure {
            class:   RUNNER_FILES_CLASS,
            message: format!("could not read `{}`: {e}", relative.display()),
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
        assert!(is_env_truthy(&env, "A"));
        assert!(!is_env_truthy(&env, "B"));
        assert!(!is_env_truthy(&env, "C"));
    }

    #[test]
    fn the_tool_cache_resolution_orders_its_defaults() {
        use frontend_gha::exprs::RUNNER_TEMP_SENTINEL;

        let store = Path::new("/store/toolcache");
        let none: BTreeMap<SmolStr, ValueOrSecretRef> = BTreeMap::new();
        let no_job: BTreeMap<String, String> = BTreeMap::new();
        let cache = |config: &BTreeMap<SmolStr, ValueOrSecretRef>,
                     job: &BTreeMap<String, String>,
                     ambient: Option<&str>,
                     store: Option<&Path>| {
            resolved_tool_cache("/w", ambient.map(String::from), store, config, job)
        };

        // Weakest default first: workspace, then store, then ambient.
        assert_eq!(cache(&none, &no_job, None, None), "/w/.ci/toolcache");
        assert_eq!(cache(&none, &no_job, None, Some(store)), "/store/toolcache");
        assert_eq!(
            cache(&none, &no_job, Some("/opt/tc"), Some(store)),
            "/opt/tc"
        );
        // An empty ambient value falls through, as the prologue's `-z` did.
        assert_eq!(cache(&none, &no_job, Some(""), None), "/w/.ci/toolcache");

        // A mid-job `GITHUB_ENV` export beats the ambient value; the step's
        // own `env:` beats them both, with its path sentinels resolved.
        let job: BTreeMap<String, String> = [("RUNNER_TOOL_CACHE".into(), "/job/tc".into())].into();
        assert_eq!(cache(&none, &job, Some("/opt/tc"), None), "/job/tc");
        let config: BTreeMap<SmolStr, ValueOrSecretRef> = [(
            SmolStr::new("RUNNER_TOOL_CACHE"),
            ValueOrSecretRef::Literal(Value::String(format!("{RUNNER_TEMP_SENTINEL}/tc"))),
        )]
        .into();
        assert_eq!(
            cache(&config, &job, Some("/opt/tc"), None),
            "/w/.ci/temp/tc"
        );
    }

    #[test]
    fn literal_markers_do_not_resolve_and_secret_values_round_trip() {
        use frontend_gha::exprs::{escape_sentinel_text, secret_sentinel};

        let literal = "\u{E000}petri-secret:NOT_A_SECRET\u{E001}";
        let secret_value = "value-\u{E002}-\u{E000}-\u{E001}";
        let process = ProcessConfig {
            run:                format!(
                "{} {}",
                escape_sentinel_text(literal),
                secret_sentinel("REAL")
            ),
            shell:              Shell::Sh,
            env:                BTreeMap::new(),
            working_dir:        None,
            soft_fail:          steps::SoftFail::default(),
            output_env_aliases: Vec::new(),
        };
        let secrets = executor::MapSecrets::from_pairs(&[("REAL", secret_value)]);
        let resolved = resolve_secret_sentinels(process, &secrets).expect("resolves");
        assert_eq!(resolved.run, format!("{literal} {secret_value}"));
    }
}
