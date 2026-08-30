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
    /// The executor's backing system refused or failed: a container engine, a
    /// cloud API, a remote agent. `backend` names it; the interface does
    /// not know the list.
    #[error("{backend} {operation} failed: {message}")]
    Backend {
        backend:   SmolStr,
        operation: SmolStr,
        message:   String,
    },
    /// `acquire`'s fence found prior work still alive that it could not safely
    /// end — the executor's kill mechanism is gone or never engaged, and
    /// nothing is ever signalled on a bare recorded id (it can be recycled
    /// to an innocent; the no-innocent-signal invariant is absolute). The
    /// scope's firings fail routably through the ordinary acquire-failure
    /// path; cleanup belongs to the operator or host policy.
    #[error("prior work from generation {generation} survived the fence: {detail}")]
    FenceLeaked {
        generation: SmolStr,
        detail:     String,
    },
    #[error("the environment is gone")]
    Gone,
}

impl EnvError {
    /// The failure class recorded when acquiring an environment fails, so a bad
    /// image or a down daemon routes like any other failure.
    pub const ACQUIRE_CLASS: &'static str = "env_acquire";

    /// A fixed discriminant, for diagnostics that may not carry the error
    /// itself. A `Backend` message is the backing system's own stderr — a
    /// container engine's, a service container's log tail — which no masker has
    /// ever seen, so a caller that cannot vouch for the variant reports this
    /// instead, beside the structural `backend` and `operation`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Workspace { .. } => "workspace",
            Self::Spawn { .. } => "spawn",
            Self::Signal(_) => "signal",
            Self::Wait(_) => "wait",
            Self::Backend { .. } => "backend",
            Self::FenceLeaked { .. } => "fence_leaked",
            Self::Gone => "gone",
        }
    }
}

/// What tearing an environment down actually managed to do.
///
/// Release is best effort and never fails a run: a leaked resource is a problem
/// to report, not a reason to lose the run's result. The resources are
/// described, not enumerated — a workspace, a container, an instance — so the
/// report is the same shape for every executor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseReport {
    /// Resources that were torn down, described (`workspace /run/x`, `container
    /// c1`).
    pub released: Vec<String>,
    /// Resources deliberately left in place, described, with why implied by
    /// policy.
    pub kept:     Vec<String>,
    pub problems: Vec<String>,
}

impl ReleaseReport {
    #[must_use]
    pub fn released(mut self, what: impl Into<String>) -> Self {
        self.released.push(what.into());
        self
    }

    #[must_use]
    pub fn kept(mut self, what: impl Into<String>) -> Self {
        self.kept.push(what.into());
        self
    }

    #[must_use]
    pub fn problem(mut self, message: impl Into<String>) -> Self {
        self.problems.push(message.into());
        self
    }

    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    /// Whether anything matching `needle` was released — `"container"`, say.
    pub fn released_any(&self, needle: &str) -> bool {
        self.released.iter().any(|r| r.contains(needle))
    }

    pub fn kept_any(&self, needle: &str) -> bool {
        self.kept.iter().any(|r| r.contains(needle))
    }
}
