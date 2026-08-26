//! Acquiring and releasing scope environments.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ir::{RuntimeSpec, ScopeId, WorkspacePolicy};
use smol_str::SmolStr;

use crate::env::ExecEnv;
use crate::error::{EnvError, ReleaseReport};

/// The default time a step gets between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10);

/// What one scope instance needs in order to exist.
#[derive(Clone, Debug)]
pub struct ScopeSpec {
    pub id: ScopeId,
    /// Instance name, which is also the workspace directory name.
    pub instance: SmolStr,
    /// The scope's env, already resolved. Secrets are not here — they are fetched at
    /// spawn time and never written down.
    pub env: BTreeMap<SmolStr, SmolStr>,
    pub runtime: RuntimeSpec,
    pub workspace: WorkspacePolicy,
    pub grace: Duration,
}

impl ScopeSpec {
    pub fn new(id: ScopeId, instance: &str) -> Self {
        Self {
            id,
            instance: SmolStr::new(instance),
            env: BTreeMap::new(),
            runtime: RuntimeSpec::host_process(),
            workspace: WorkspacePolicy::Shared,
            grace: DEFAULT_GRACE,
        }
    }

    pub fn with_env(mut self, env: BTreeMap<SmolStr, SmolStr>) -> Self {
        self.env = env;
        self
    }

    pub fn with_runtime(mut self, runtime: RuntimeSpec) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }
}

/// Whether the work in a scope succeeded, which decides workspace retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeOutcome {
    Succeeded,
    Failed,
}

/// When to keep a workspace after release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Retention {
    Always,
    /// The default: a failed scope's workspace is what you need to debug it.
    #[default]
    OnFailure,
    Never,
}

impl Retention {
    pub fn keeps(self, outcome: ScopeOutcome) -> bool {
        match self {
            Retention::Always => true,
            Retention::Never => false,
            Retention::OnFailure => outcome == ScopeOutcome::Failed,
        }
    }
}

/// What it takes to tear one environment down.
///
/// Each executor defines its own — a workspace path and a retention policy on the
/// host, a container name besides under Docker — and gets it back, untouched, in
/// [`Executor::release`]. The interface only carries it and never looks inside, so a
/// new kind of environment needs no change here. Any `Debug + Send + Sync + 'static`
/// type qualifies: an executor derives `Debug` on a struct and passes it to
/// [`EnvHandle::new`].
pub trait Teardown: Any + std::fmt::Debug + Send + Sync {}

impl<T: Any + std::fmt::Debug + Send + Sync> Teardown for T {}

/// A live environment, and what it takes to get rid of it.
#[derive(Clone)]
pub struct EnvHandle {
    scope: ScopeId,
    instance: SmolStr,
    env: Arc<dyn ExecEnv>,
    teardown: Arc<dyn Teardown>,
}

impl EnvHandle {
    pub fn new(
        scope: ScopeId,
        instance: SmolStr,
        env: Arc<dyn ExecEnv>,
        teardown: impl Teardown,
    ) -> Self {
        Self {
            scope,
            instance,
            env,
            teardown: Arc::new(teardown),
        }
    }

    pub fn scope(&self) -> ScopeId {
        self.scope
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The capability handed to step kinds.
    pub fn exec(&self) -> Arc<dyn ExecEnv> {
        Arc::clone(&self.env)
    }

    /// The executor's own teardown record, when this handle was made by an executor
    /// that uses `T`. `None` means the handle came from some other executor.
    pub fn teardown<T: Teardown>(&self) -> Option<&T> {
        // Deref to the trait object before upcasting. `Arc<dyn Teardown>` is itself a
        // `Teardown`, and a method call on the `Arc` would resolve there and downcast
        // the wrapper instead of what it holds.
        let record: &dyn Any = &*self.teardown;
        record.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for EnvHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvHandle")
            .field("scope", &self.scope)
            .field("instance", &self.instance)
            .field("teardown", &self.teardown)
            .finish()
    }
}

/// Materializes environments. Knows nothing about what steps mean.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Materialize the scope's environment.
    ///
    /// **`acquire` fences prior work** (§9): when it returns, no process from a
    /// previous acquisition of this scope can still mutate the workspace or be
    /// observed as this environment's status. A driver crash does not kill
    /// running steps — release owns cleanup, and remote sandboxes outlive
    /// workers by design — so resume leans on this fence before re-dispatching.
    /// The stock executors implement it (the host executor's generation-scoped
    /// sentinel protocol; Docker's remove-by-deterministic-name); a remote
    /// provider implements its own reconnect-and-fence here. Work that cannot
    /// be safely ended fails the acquire with [`EnvError::FenceLeaked`] rather
    /// than signalling an unverified id. The fence is idempotent, and it covers
    /// the workspace only: side effects outside it (network calls, pushes) may
    /// have happened in the crashed attempt and happen again — resume is
    /// at-least-once for external side effects.
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError>;

    /// Tear down. Idempotent, and never fails the run: problems are reported.
    ///
    /// `outcome` decides workspace retention, which the handoff's signature has no
    /// room for — a scope cannot know on its own whether its work failed.
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport;
}
