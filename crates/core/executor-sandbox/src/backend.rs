//! Which sandbox-driver provider a scope's environment runs on.

/// The backend a [`crate::SandboxExecutor`] drives. It selects how a
/// [`ir::RuntimeTarget`] becomes a `SandboxSpec` and how the workspace and
/// the driver's address are reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Directories and real processes on this machine.
    Host,
    /// Containers on a local Docker daemon.
    Docker,
}

impl BackendKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
        }
    }
}
