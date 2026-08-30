//! Live visibility into acquisition and container work.
//!
//! Nothing here touches the replay log: image pulls, builds, and service
//! lifecycle are wall-clock effects, and the log stays byte-identical whether
//! anyone watched them. A failed or unhealthy service still surfaces
//! deterministically, as a scope-acquire failure.

use ir::ScopeId;
use smol_str::SmolStr;

/// One live event from an executor's effect work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// An image is being pulled from a registry.
    PullingImage { image: SmolStr },
    /// An image is being built from a workspace Dockerfile; `tag` is the cache
    /// key.
    BuildingImage { tag: SmolStr },
    /// A service container was created and started.
    ServiceStarted { name: SmolStr },
    /// A service reported healthy (or has no health check and is running).
    ServiceHealthy { name: SmolStr },
}

/// Where live acquisition events go. Handed to [`Executor::acquire`] through
/// the [`AcquireContext`]; a scope-bound [`ContainerRunner`] may keep it for
/// the pulls and builds its one-shot containers need.
///
/// [`Executor::acquire`]: crate::Executor::acquire
/// [`AcquireContext`]: crate::AcquireContext
/// [`ContainerRunner`]: crate::ContainerRunner
pub trait ProgressSink: Send + Sync {
    fn progress(&self, scope: ScopeId, event: Progress);
}

/// The default sink: silence.
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn progress(&self, _scope: ScopeId, _event: Progress) {}
}
