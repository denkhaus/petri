//! The environment a step runs in, and the process it runs.
//!
//! An [`ExecEnv`] is a capability handed to a step kind. The process step is written
//! once against it and never mentions Docker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use tokio::sync::mpsc;

use crate::error::EnvError;

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: SmolStr,
    pub args: Vec<SmolStr>,
    pub env: BTreeMap<SmolStr, SmolStr>,
    /// Relative to the workspace root.
    pub cwd: Option<PathBuf>,
}

impl ProcessSpec {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: SmolStr::new(program),
            args: args.iter().map(|a| SmolStr::new(*a)).collect(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    pub fn with_env(mut self, env: BTreeMap<SmolStr, SmolStr>) -> Self {
        self.env = env;
        self
    }

    pub fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }
}

/// One captured output line, tagged with the stream it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub stream: ir::LogStream,
    pub line: String,
    /// The line hit the size cap and was cut.
    pub truncated: bool,
}

/// Captured output, merged across both streams in arrival order.
///
/// One stream rather than the separate `stdout()` / `stderr()` of the handoff: the
/// requirement is that lines are emitted in arrival order, and merging two receivers
/// after the fact cannot recover an order that was never recorded. Each line carries
/// its stream tag, so nothing is lost.
pub type LineStream = mpsc::Receiver<LogLine>;

/// The signals this package sends. Always to a process **group**, never to a pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    Term,
    Kill,
}

impl Sig {
    pub fn number(self) -> i32 {
        match self {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sig::Term => "SIGTERM",
            Sig::Kill => "SIGKILL",
        }
    }
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    /// The signal that killed it, when it was killed.
    pub signal: Option<i32>,
}

impl ExitStatus {
    pub fn code(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }

    pub fn signalled(signal: i32) -> Self {
        Self {
            code: None,
            signal: Some(signal),
        }
    }

    pub fn success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }
}

/// The mapping every executor needs when the process it waited on was a real child
/// of this one: an exit code when there is one, otherwise the signal that killed it.
impl From<std::process::ExitStatus> for ExitStatus {
    fn from(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt;
        match (status.code(), status.signal()) {
            (Some(code), _) => ExitStatus::code(code),
            (None, Some(signal)) => ExitStatus::signalled(signal),
            (None, None) => ExitStatus::code(-1),
        }
    }
}

/// A running process, addressed as a process group.
#[async_trait]
pub trait ProcessHandle: Send {
    /// The merged, stream-tagged output. Available once; later calls return `None`.
    fn lines(&mut self) -> Option<LineStream>;

    async fn wait(&mut self) -> Result<ExitStatus, EnvError>;

    /// Signal the process **group**. Idempotent: signalling an already-dead group
    /// is not an error.
    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError>;
}

/// The capability a step kind receives: somewhere to run a process, and a workspace
/// reached only through this interface.
///
/// Nothing here assumes the workspace is on the machine the driver runs on. A step
/// kind that needs a file in the workspace asks the environment for it, and an
/// executor whose workspace is remote — a cloud instance, an agent elsewhere — answers
/// over whatever transport it has. The two local executors answer from the filesystem.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError>;

    /// The workspace root as a process running in this environment sees it, for
    /// building paths to hand to the process.
    fn workspace_path(&self) -> &str;

    /// Read a workspace-relative file. `Ok(None)` when it does not exist.
    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError>;

    /// Read at most `limit` bytes from a workspace-relative file. Implementations
    /// should stop reading once the limit is exceeded. The default preserves
    /// compatibility for remote executors and still rejects an oversized result.
    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let Some(bytes) = self.read_file(relative).await? else {
            return Ok(None);
        };
        if bytes.len() > limit {
            return Err(EnvError::Workspace {
                path: relative.display().to_string(),
                message: format!("file exceeds the {limit}-byte read limit"),
            });
        }
        Ok(Some(bytes))
    }

    /// Write a workspace-relative file, creating parent directories.
    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError>;

    /// How long a step gets between `SIGTERM` and `SIGKILL`.
    fn grace(&self) -> Duration;

    /// How a process in *this* environment reaches the driver's machine — the
    /// host half of a URL for a service the driver runs beside the workspace.
    /// One fact, answered where it is known: a host process uses loopback; a
    /// containerized environment answers with the alias its executor
    /// guaranteed resolvable at create (`host.docker.internal`).
    fn host_address(&self) -> &str {
        "127.0.0.1"
    }

    /// One variable of the environment a process spawned here starts from,
    /// before the spec's own `env` lands on top: a container's effective env,
    /// snapshotted at create (the image's plus the scope's); a host process's
    /// inherited env under the scope's. A fact, not policy — `None` means this
    /// environment does not carry the variable, which is all the default can
    /// promise for an executor that never looked.
    fn ambient_env(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }

    /// Whether an absolute path on the driver's machine names the same file in
    /// this environment. True only for a host process: a container shares
    /// nothing with the host but the workspace (mounted at its own path), and
    /// a remote environment shares nothing at all — so `false` is the default.
    fn shares_host_filesystem(&self) -> bool {
        false
    }
}
