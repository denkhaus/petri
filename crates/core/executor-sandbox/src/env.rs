//! [`ExecEnv`] and its process handle over one [`Sandbox`].
//!
//! A step spawn becomes one `run_streaming` call in an owned task. Output
//! chunks are fed through the interface crate's own line pump (a duplex pipe
//! per stream), so the 64 KiB line cap and truncation marker match every
//! other executor exactly. The polite cancellation ladder maps to the
//! sandbox-driver two-level stop: `SIGTERM` triggers the cancel token with
//! the scope's grace, `SIGKILL` triggers the kill token.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::{LINE_CHANNEL_CAPACITY, pump};
use executor::{
    EnvError, ExecEnv, ExitStatus, LineStream, ProcessHandle, ProcessSpec, Sig, StdinMode,
    StdinWriter,
};
use sandbox_driver::{ExecControls, ExecSpec, OutputStream, Sandbox, StdinSource, Termination};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Bytes of pipe buffer between the output sink and each line pump.
const OUTPUT_PIPE_CAPACITY: usize = 64 * 1024;

/// One live sandbox, handed to step kinds as their spawn capability.
pub(crate) struct SandboxEnv {
    sandbox:        Arc<dyn Sandbox>,
    /// The workspace path as a process in the sandbox sees it.
    workspace:      String,
    /// The same workspace on the host: the bind-mount source. Workspace file
    /// I/O goes here directly, not through the sandbox, so it is one write
    /// away and never races the container's view of the mount.
    workspace_host: PathBuf,
    /// The driver's machine as a process in the sandbox reaches it.
    host_address:   String,
    /// The effective environment, read once at acquire.
    ambient:        BTreeMap<String, String>,
    grace:          Duration,
}

impl SandboxEnv {
    pub(crate) fn new(
        sandbox: Arc<dyn Sandbox>,
        workspace: String,
        workspace_host: PathBuf,
        host_address: String,
        ambient: BTreeMap<String, String>,
        grace: Duration,
    ) -> Self {
        Self {
            sandbox,
            workspace,
            workspace_host,
            host_address,
            ambient,
            grace,
        }
    }
}

/// Quotes one argument for a Bash command line with single quotes.
fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

/// Builds the `exec 'prog' 'arg'...` command line the Bash contract runs.
fn exec_command(spec: &ProcessSpec) -> String {
    let mut command = String::from("exec ");
    command.push_str(&shell_quote(&spec.program));
    for arg in &spec.args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

/// Maps a finished `run_streaming` to the executor's exit status. A signal
/// wins over a code, so a foreign signal reads the same on every backend.
fn exit_status(termination: Termination, code: Option<i32>, signal: Option<i32>) -> ExitStatus {
    if let Some(signal) = signal {
        return ExitStatus::signalled(signal);
    }
    match termination {
        Termination::Killed => ExitStatus::signalled(libc_sigkill()),
        // The driver's own ladder decides cancel vs timeout; the handle only
        // needs a plausible signalled status for a stop it did not exit from.
        Termination::Cancelled | Termination::TimedOut => ExitStatus::signalled(libc_sigterm()),
        // Exited, Unknown, and any future variant: report the code as-is.
        _ => ExitStatus { code, signal: None },
    }
}

fn libc_sigkill() -> i32 {
    9
}
fn libc_sigterm() -> i32 {
    15
}

#[async_trait]
impl ExecEnv for SandboxEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let command = exec_command(&spec);
        // `docker exec -w` refuses a directory that does not exist yet (`repo/`
        // before the first checkout), so create the step's cwd through the
        // bind mount first, as the workspace root itself already is.
        if let Some(cwd) = &spec.cwd {
            let host_cwd = self.workspace_host.join(cwd);
            fs::create_dir_all(&host_cwd)
                .await
                .map_err(|error| EnvError::workspace("create", host_cwd.display(), error))?;
        }
        let working_dir = spec
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().into_owned());

        let mut exec_spec = ExecSpec::new(command).no_timeout();
        if let Some(dir) = working_dir {
            exec_spec = exec_spec.working_dir(dir);
        }
        for (key, value) in &spec.env {
            exec_spec = exec_spec.env_var(key.as_str(), value.as_str());
        }

        // Stdin: a piped step gets a writer whose read half streams into the
        // command for its whole life.
        let stdin_writer = match spec.stdin {
            StdinMode::Null => None,
            StdinMode::Piped => {
                let (writer, reader) = duplex(OUTPUT_PIPE_CAPACITY);
                let source = StdinSource::new(reader);
                Some((Box::new(writer) as StdinWriter, source))
            }
        };

        // Output: one duplex per stream feeds the interface line pump, so the
        // line cap and truncation marker are the shared ones.
        let (line_tx, line_rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        let (stdout_writer, stdout_reader) = duplex(OUTPUT_PIPE_CAPACITY);
        let (stderr_writer, stderr_reader) = duplex(OUTPUT_PIPE_CAPACITY);
        tokio::spawn(pump(stdout_reader, ir::LogStream::Stdout, line_tx.clone()));
        tokio::spawn(pump(stderr_reader, ir::LogStream::Stderr, line_tx));

        let cancel = CancellationToken::new();
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
            cancel:                Some(cancel.clone()),
            kill:                  Some(kill.clone()),
            grace:                 Some(self.grace),
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

        let sandbox = self.sandbox.clone();
        tokio::spawn(async move {
            let outcome = sandbox.exec().run_streaming(&exec_spec, controls).await;
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

        Ok(Box::new(SandboxProcess {
            lines: Some(line_rx),
            stdin: stdin_writer.map(|(writer, _)| writer),
            cancel,
            kill,
            grace: self.grace,
            status: status_rx,
            cached: None,
        }))
    }

    fn workspace_path(&self) -> &str {
        &self.workspace
    }

    fn host_address(&self) -> &str {
        &self.host_address
    }

    fn ambient_env(&self, name: &str) -> Option<String> {
        self.ambient.get(name).cloned()
    }

    fn shares_host_filesystem(&self) -> bool {
        // A container sees only the bind-mounted workspace, never host paths.
        false
    }

    // Workspace files live on the host bind mount, so these go straight to
    // the host path — the same bytes the container sees, one write away.

    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        match fs::read(self.workspace_host.join(relative)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(EnvError::workspace("read", relative.display(), error)),
        }
    }

    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let file = match fs::File::open(self.workspace_host.join(relative)).await {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(EnvError::workspace("open", relative.display(), error)),
        };
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| EnvError::workspace("read", relative.display(), error))?;
        if bytes.len() > limit {
            return Err(EnvError::workspace(
                "read",
                relative.display(),
                executor::oversized_read(limit),
            ));
        }
        Ok(Some(bytes))
    }

    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
        let path = self.workspace_host.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| EnvError::workspace("create", parent.display(), error))?;
        }
        fs::write(&path, contents)
            .await
            .map_err(|error| EnvError::workspace("write", relative.display(), error))
    }

    fn grace(&self) -> Duration {
        self.grace
    }
}

/// The process handle a step drives: its output, its stdin, its wait, and the
/// two-level cancellation ladder.
struct SandboxProcess {
    lines:  Option<LineStream>,
    stdin:  Option<StdinWriter>,
    cancel: CancellationToken,
    kill:   CancellationToken,
    grace:  Duration,
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
                let value = result.map_err(|message| EnvError::Backend {
                    backend: smol_str::SmolStr::new("sandbox"),
                    operation: smol_str::SmolStr::new("exec"),
                    message,
                })?;
                self.cached = Some(value);
                return Ok(value);
            }
            if status.changed().await.is_err() {
                return Err(EnvError::Gone);
            }
        }
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        let _ = self.grace;
        match sig {
            Sig::Term => self.cancel.cancel(),
            Sig::Kill => self.kill.cancel(),
        }
        Ok(())
    }
}
