//! Host executor convenience wrapper over the standard plugin router.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};

use crate::RoutingExecutor;
use crate::run::workspace_dir;

/// Runs host scopes through the sandbox-driver Host plugin. The provider
/// owns the durable registry, workspace, sentinel, and crash fence.
pub struct HostExecutor {
    router: RoutingExecutor,
}

impl HostExecutor {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            router: RoutingExecutor::local(run_dir, Retention::default()),
        }
    }

    #[must_use]
    pub fn with_retention(self, retention: Retention) -> Self {
        Self {
            router: RoutingExecutor::local(self.run_dir(), retention),
        }
    }

    pub fn run_dir(&self) -> &Path {
        self.router.identity().run_dir()
    }

    pub fn workspace_for(&self, workspace_id: &str) -> PathBuf {
        workspace_dir(self.run_dir(), workspace_id)
    }
}

#[async_trait]
impl Executor for HostExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        self.router.acquire(scope, ctx).await
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        self.router.release(env, outcome).await
    }
}
