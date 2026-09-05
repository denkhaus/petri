//! [`ExecEnv`] and [`ContainerRunner`] over one [`Sandbox`].
//!
//! A step spawn becomes one `run_streaming` call in an owned task; a
//! Docker action becomes one `OneShot::run` call the same way. Output
//! chunks are fed through the interface crate's own line pump (a duplex
//! pipe per stream), so the 64 KiB line cap and truncation marker match
//! every other executor exactly. The step's cancellation ladder is the only
//! one: `SIGTERM` fires the sandbox-driver `term` token, `SIGKILL` fires
//! `kill`, and the provider sends exactly that signal and nothing more.
//!
//! Workspace files go through the sandbox's filesystem facet. The
//! workspace lives inside the sandbox — a volume it owns — and nothing on
//! Petri's machine mirrors it, so this is the only path to it, on a local
//! daemon and a remote one alike.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::{LINE_CHANNEL_CAPACITY, pump};
use executor::{
    ContainerImage, ContainerRunner, EnvError, ExecEnv, ExitStatus, LineStream, OneShotContainer,
    ProcessHandle, ProcessSpec, Sig, StdinMode, StdinWriter,
};
use sandbox_driver::{
    Error as DriverError, ExecControls, ExecSpec, ExecStreamingResult, OneShotImage, OneShotSpec,
    OutputStream, Sandbox, StdinSource, Termination,
};
use smol_str::SmolStr;
use tokio::io::{AsyncWriteExt, duplex};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::BACKEND;

/// Bytes of pipe buffer between the output sink and each line pump.
const OUTPUT_PIPE_CAPACITY: usize = 64 * 1024;
/// How long a mkdir for a step's cwd may take.
const MKDIR_TIMEOUT: Duration = Duration::from_secs(30);

/// One live sandbox, handed to step kinds as their spawn capability.
pub(crate) struct SandboxEnv {
    pub(crate) sandbox:      Arc<dyn Sandbox>,
    /// The workspace as the sandbox sees it: its working directory.
    pub(crate) workspace:    String,
    /// The effective environment, read once at acquire.
    pub(crate) ambient:      BTreeMap<String, String>,
    /// This holder's scope environment and container options, over the
    /// sandbox's original environment and under the process's own.
    pub(crate) env:          BTreeMap<SmolStr, SmolStr>,
    pub(crate) grace:        Duration,
    /// How a process in the sandbox reaches services on Petri's machine.
    pub(crate) host_address: Option<String>,
}

/// Maps a finished `run_streaming` to the executor's exit status. The
/// observed signal wins over a code, so a foreign signal and the step's own
/// ladder read the same on every backend. The fallbacks below are for a
/// provider that stopped the command but could not observe how (Daytona ends
/// a session without seeing the child's status).
fn exit_status(termination: Termination, code: Option<i32>, signal: Option<i32>) -> ExitStatus {
    if let Some(signal) = signal {
        return ExitStatus::signalled(signal);
    }
    match termination {
        Termination::Killed | Termination::TimedOut => ExitStatus::signalled(Sig::Kill.number()),
        // The step's ladder decides cancel vs timeout; the handle only needs a
        // plausible signalled status for a stop it did not exit from.
        Termination::Cancelled => ExitStatus::signalled(Sig::Term.number()),
        // Exited, Unknown, and any future variant: report the code as-is.
        _ => ExitStatus { code, signal: None },
    }
}

/// What one streamed job runs: a step's exec, or an action's container.
enum Job {
    Exec(ExecSpec),
    OneShot(OneShotSpec),
}

/// Runs `job` in an owned task with the interface's line pumps on its
/// output and the step's stop tokens on its controls.
fn spawn_streamed(sandbox: Arc<dyn Sandbox>, job: Job, stdin: bool) -> SandboxProcess {
    // Stdin: a piped step gets a writer whose read half streams into the
    // command for its whole life.
    let stdin_writer = stdin.then(|| {
        let (writer, reader) = duplex(OUTPUT_PIPE_CAPACITY);
        let source = StdinSource::new(reader);
        (Box::new(writer) as StdinWriter, source)
    });

    // Output: one duplex per stream feeds the interface line pump, so the
    // line cap and truncation marker are the shared ones.
    let (line_tx, line_rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
    let (stdout_writer, stdout_reader) = duplex(OUTPUT_PIPE_CAPACITY);
    let (stderr_writer, stderr_reader) = duplex(OUTPUT_PIPE_CAPACITY);
    tokio::spawn(pump(stdout_reader, ir::LogStream::Stdout, line_tx.clone()));
    tokio::spawn(pump(stderr_reader, ir::LogStream::Stderr, line_tx));

    let term = CancellationToken::new();
    let kill = CancellationToken::new();
    let (status_tx, status_rx) = watch::channel(None);

    // The sink writes each chunk into the matching stream pipe; the pumps
    // read those pipes and split into lines. The writers are shared into
    // the closure so the task can shut them for EOF when the run ends.
    let stdout_slot = Arc::new(Mutex::new(Some(stdout_writer)));
    let stderr_slot = Arc::new(Mutex::new(Some(stderr_writer)));
    let sink_stdout = stdout_slot.clone();
    let sink_stderr = stderr_slot.clone();
    let stdin_source = stdin_writer.as_ref().map(|(_, source)| source.clone());
    let controls = ExecControls {
        term:                  Some(term.clone()),
        kill:                  Some(kill.clone()),
        stdin:                 stdin_source,
        sink:                  Some(Arc::new(move |stream, chunk| {
            let slot = match stream {
                OutputStream::Stdout => sink_stdout.clone(),
                OutputStream::Stderr => sink_stderr.clone(),
            };
            Box::pin(async move {
                let mut guard = slot.lock().await;
                if let Some(writer) = guard.as_mut() {
                    let _ = writer.write_all(&chunk).await;
                }
                Ok(())
            })
        })),
        retained_output_limit: Some(0),
    };

    tokio::spawn(async move {
        let outcome: Result<ExecStreamingResult, DriverError> = match &job {
            Job::Exec(spec) => sandbox.exec().run_streaming(spec, controls).await,
            Job::OneShot(spec) => match sandbox.one_shot() {
                Some(one_shot) => one_shot.run(spec, controls).await,
                None => Err(DriverError::unsupported(
                    sandbox_driver::Capability::OneShot,
                )),
            },
        };
        // Close the pipes so the pumps flush and end.
        if let Some(mut writer) = stdout_slot.lock().await.take() {
            let _ = writer.shutdown().await;
        }
        if let Some(mut writer) = stderr_slot.lock().await.take() {
            let _ = writer.shutdown().await;
        }
        let status: Result<ExitStatus, String> = match outcome {
            Ok(streaming) => Ok(exit_status(
                streaming.result.termination,
                streaming.result.exit_code,
                streaming.result.signal,
            )),
            Err(error) => Err(error.to_string()),
        };
        let _ = status_tx.send(Some(status));
    });

    SandboxProcess {
        lines: Some(line_rx),
        stdin: stdin_writer.map(|(writer, _)| writer),
        term,
        kill,
        status: status_rx,
        cached: None,
    }
}

/// Maps a facet failure to the executor's error.
fn facet_error(operation: &str, error: &DriverError) -> EnvError {
    EnvError::backend(BACKEND, operation, error.to_string())
}

#[async_trait]
impl ExecEnv for SandboxEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        // `docker exec -w` refuses a directory that does not exist yet
        // (`repo/` before the first checkout), so create the step's cwd
        // first, with the one command every image contract provides.
        if let Some(cwd) = &spec.cwd {
            let cwd = cwd.to_string_lossy().into_owned();
            let mkdir = ExecSpec::new("mkdir")
                .args(["-p", cwd.as_str()])
                .timeout(MKDIR_TIMEOUT);
            let result = self
                .sandbox
                .exec()
                .run(&mkdir)
                .await
                .map_err(|error| facet_error("mkdir", &error))?;
            if !result.success() {
                return Err(EnvError::backend(
                    BACKEND,
                    "mkdir",
                    format!("could not create `{cwd}`: {}", result.stderr_lossy().trim()),
                ));
            }
        }
        let working_dir = spec
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().into_owned());

        // The step's program and arguments go to the sandbox as given: the
        // exec contract is an argument vector, so nothing is quoted or
        // interpreted on the way.
        let mut exec_spec = ExecSpec::new(spec.program.as_str())
            .args(spec.args.iter().map(SmolStr::as_str))
            .no_timeout();
        if let Some(dir) = working_dir {
            exec_spec = exec_spec.working_dir(dir);
        }
        for (key, value) in self
            .env
            .iter()
            .filter(|(key, _)| !spec.env.contains_key(*key))
            .chain(spec.env.iter())
        {
            exec_spec = exec_spec.env_var(key.as_str(), value.as_str());
        }
        let stdin = matches!(spec.stdin, StdinMode::Piped);
        Ok(Box::new(spawn_streamed(
            self.sandbox.clone(),
            Job::Exec(exec_spec),
            stdin,
        )))
    }

    fn workspace_path(&self) -> &str {
        &self.workspace
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        self.host_address
            .as_deref()
            .ok_or(EnvError::HostUnreachable)
    }

    fn ambient_env(&self, name: &str) -> Option<String> {
        self.env
            .get(name)
            .map(ToString::to_string)
            .or_else(|| self.ambient.get(name).cloned())
    }

    fn shares_host_filesystem(&self) -> bool {
        // A sandbox owns its workspace; nothing of Petri's machine is in it.
        false
    }

    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        match self.sandbox.fs().read(&relative.to_string_lossy()).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(DriverError::NotFound { .. }) => Ok(None),
            Err(error) => Err(facet_error("read", &error)),
        }
    }

    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        // One byte past the limit is enough to know the file is too big,
        // without ever pulling the whole of it across.
        let bounded = limit.saturating_add(1) as u64;
        match self
            .sandbox
            .fs()
            .read_range(&relative.to_string_lossy(), 0, Some(bounded))
            .await
        {
            Ok(bytes) if bytes.len() > limit => Err(EnvError::workspace(
                "read",
                relative.display(),
                executor::oversized_read(limit),
            )),
            Ok(bytes) => Ok(Some(bytes)),
            Err(DriverError::NotFound { .. }) => Ok(None),
            Err(error) => Err(facet_error("read", &error)),
        }
    }

    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
        self.sandbox
            .fs()
            .write(&relative.to_string_lossy(), contents)
            .await
            .map_err(|error| facet_error("write", &error))
    }

    fn grace(&self) -> Duration {
        self.grace
    }
}

/// The process handle a step drives: its output, its stdin, its wait, and the
/// two raw stop signals the step's ladder fires.
struct SandboxProcess {
    lines:  Option<LineStream>,
    stdin:  Option<StdinWriter>,
    term:   CancellationToken,
    kill:   CancellationToken,
    status: watch::Receiver<Option<Result<ExitStatus, String>>>,
    cached: Option<ExitStatus>,
}

#[async_trait]
impl ProcessHandle for SandboxProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    fn stdin(&mut self) -> Option<StdinWriter> {
        self.stdin.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        if let Some(status) = self.cached {
            return Ok(status);
        }
        let mut status = self.status.clone();
        loop {
            let current = status.borrow_and_update().clone();
            if let Some(result) = current {
                let value =
                    result.map_err(|message| EnvError::backend(BACKEND, "exec", message))?;
                self.cached = Some(value);
                return Ok(value);
            }
            if status.changed().await.is_err() {
                return Err(EnvError::Gone);
            }
        }
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        match sig {
            Sig::Term => self.term.cancel(),
            Sig::Kill => self.kill.cancel(),
        }
        Ok(())
    }
}

/// One-shot containers in a sandbox's world: the provider's own operation,
/// which shares the sandbox's workspace and network namespace, streams
/// through the same pumps, and is ended by the sandbox's stop and delete.
pub(crate) struct OneShotRunner {
    pub(crate) sandbox:      Arc<dyn Sandbox>,
    pub(crate) workspace:    String,
    pub(crate) host_address: Option<String>,
    /// The scope's env, under the spec's own.
    pub(crate) env:          BTreeMap<SmolStr, SmolStr>,
}

#[async_trait]
impl ContainerRunner for OneShotRunner {
    fn workspace_path(&self) -> &str {
        &self.workspace
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        self.host_address
            .as_deref()
            .ok_or(EnvError::HostUnreachable)
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        if self.sandbox.one_shot().is_none() {
            return Err(EnvError::backend(
                BACKEND,
                "one-shot",
                "this sandbox provider runs no one-shot containers",
            ));
        }
        let image = match spec.image {
            ContainerImage::Registry { image } => OneShotImage::Registry {
                reference: image.to_string(),
            },
            ContainerImage::Build {
                context,
                dockerfile,
                tag,
                reuse,
            } => OneShotImage::Build {
                context: context.to_string_lossy().into_owned(),
                dockerfile: dockerfile.map(|file| file.to_string()),
                tag: tag.to_string(),
                reuse,
            },
        };
        let mut one_shot = OneShotSpec::new(image);
        if let Some(entrypoint) = spec.entrypoint {
            one_shot = one_shot.entrypoint(entrypoint.as_str());
        }
        one_shot = one_shot.args(spec.args.iter().map(SmolStr::as_str));
        // The scope's env first, the spec's own on top.
        for (key, value) in self
            .env
            .iter()
            .filter(|(key, _)| !spec.env.contains_key(*key))
            .chain(spec.env.iter())
        {
            one_shot = one_shot.env_var(key.as_str(), value.as_str());
        }
        if let Some(workdir) = spec.workdir {
            one_shot = one_shot.working_dir(workdir.as_str());
        }
        Ok(Box::new(spawn_streamed(
            self.sandbox.clone(),
            Job::OneShot(one_shot),
            false,
        )))
    }
}
