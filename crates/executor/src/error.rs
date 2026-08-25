//! Failures from materializing or using an environment.

use smol_str::SmolStr;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvError {
    #[error("could not create the workspace at {path}: {message}")]
    Workspace { path: String, message: String },
    #[error("could not spawn `{program}`: {message}")]
    Spawn { program: SmolStr, message: String },
    #[error("could not signal the process group: {0}")]
    Signal(String),
    #[error("waiting on the process failed: {0}")]
    Wait(String),
    #[error("docker {command} failed: {message}")]
    Docker { command: SmolStr, message: String },
    #[error("the environment is gone")]
    Gone,
}

impl EnvError {
    /// The failure class recorded when acquiring an environment fails, so a bad
    /// image or a down daemon routes like any other failure.
    pub const ACQUIRE_CLASS: &'static str = "env_acquire";
}

/// What tearing an environment down actually managed to do.
///
/// Release is best effort and never fails a run: a leaked container is a problem to
/// report, not a reason to lose the run's result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseReport {
    pub workspace_removed: bool,
    pub workspace_kept: Option<String>,
    pub container_removed: bool,
    pub problems: Vec<String>,
}

impl ReleaseReport {
    pub fn problem(mut self, message: impl Into<String>) -> Self {
        self.problems.push(message.into());
        self
    }

    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}
