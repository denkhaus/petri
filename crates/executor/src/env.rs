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

/// The capability a step kind receives: somewhere to run a process, and a workspace.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError>;

    /// Host-visible workspace path. For Docker this is the bind-mount source, so
    /// artifacts and logs are handled the same way either side.
    fn workspace(&self) -> &Path;

    /// The same location as the step sees it. Identical to [`ExecEnv::workspace`] on
    /// the host; the in-container path under Docker.
    fn workspace_in_env(&self) -> &str;

    /// How long a step gets between `SIGTERM` and `SIGKILL`.
    fn grace(&self) -> Duration;
}
