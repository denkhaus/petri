//! Acquiring and releasing scope environments.

use std::collections::BTreeMap;
use std::path::PathBuf;
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

/// How to tear a particular environment down.
#[derive(Clone, Debug)]
pub enum Teardown {
    HostWorkspace {
        path: PathBuf,
        retention: Retention,
    },
    DockerContainer {
        container: String,
        path: PathBuf,
        retention: Retention,
        grace: Duration,
    },
}

/// A live environment, and what it takes to get rid of it.
#[derive(Clone)]
pub struct EnvHandle {
    scope: ScopeId,
    instance: SmolStr,
    env: Arc<dyn ExecEnv>,
    teardown: Teardown,
}

impl EnvHandle {
    pub fn new(
        scope: ScopeId,
        instance: SmolStr,
        env: Arc<dyn ExecEnv>,
        teardown: Teardown,
    ) -> Self {
        Self {
            scope,
            instance,
            env,
            teardown,
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

    pub fn teardown(&self) -> &Teardown {
        &self.teardown
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
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError>;

    /// Tear down. Idempotent, and never fails the run: problems are reported.
    ///
    /// `outcome` decides workspace retention, which the handoff's signature has no
    /// room for — a scope cannot know on its own whether its work failed.
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport;
}
