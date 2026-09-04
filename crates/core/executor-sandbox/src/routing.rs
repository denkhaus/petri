//! An [`Executor`] that routes each scope to the backend its runtime target
//! needs: a host process to the native [`HostExecutor`], a container to the
//! Docker [`SandboxExecutor`]. It is the composition the runtime registers for
//! a run.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::SandboxProvider;
use sandbox_driver_docker::DockerProvider;
use tokio::sync::OnceCell;

use crate::host::HostTeardown;
use crate::oneshot::{self, OneShotRunner};
use crate::run::{RunIdentity, scope_dir, workspace_dir};
use crate::{HostExecutor, SandboxExecutor, SandboxTeardown};

/// Routes scopes to the native host executor or the Docker [`SandboxExecutor`]
/// by runtime target.
///
/// A host-process scope runs as real processes on this machine, with the
/// sentinel and crash fence a bare process needs; a container scope runs on
/// the Docker provider. A run with no Docker daemon still serves its
/// host-process scopes, and a container scope then fails routably at acquire.
pub struct RoutingExecutor {
    host:      HostExecutor,
    /// The Docker executor, or `None` when no daemon is reachable. Filled
    /// eagerly by [`RoutingExecutor::new`], or on the first container scope by
    /// [`RoutingExecutor::local`], so a host-only run never touches a daemon.
    docker:    OnceCell<Option<SandboxExecutor>>,
    identity:  Arc<RunIdentity>,
    retention: Retention,
}

impl RoutingExecutor {
    /// Builds a router. `docker_provider` is optional so a host-only run needs
    /// no daemon; both backends root their workspaces under `run_dir`.
    pub fn new(
        docker_provider: Option<Arc<dyn SandboxProvider>>,
        run_dir: impl Into<PathBuf>,
        retention: Retention,
    ) -> Self {
        let router = Self::over(run_dir, retention);
        let docker = docker_provider.map(|provider| router.sandbox_executor(provider));
        Self {
            docker: OnceCell::new_with(Some(docker)),
            ..router
        }
    }

    /// A router over this machine: the native host executor, and the local
    /// Docker daemon connected on the first container scope. The standard
    /// runtime's default.
    pub fn local(run_dir: impl Into<PathBuf>, retention: Retention) -> Self {
        Self::over(run_dir, retention)
    }

    /// The host side of a router, with the Docker side unconnected.
    fn over(run_dir: impl Into<PathBuf>, retention: Retention) -> Self {
        let run_dir = run_dir.into();
        Self {
            host: HostExecutor::new(run_dir.clone()).with_retention(retention),
            docker: OnceCell::new(),
            identity: Arc::new(RunIdentity::new(run_dir)),
            retention,
        }
    }

    fn sandbox_executor(&self, provider: Arc<dyn SandboxProvider>) -> SandboxExecutor {
        SandboxExecutor::with_identity(provider, self.identity.clone(), self.retention)
    }

    /// The Docker executor, connecting to the local daemon on first use when
    /// the router was built with [`RoutingExecutor::local`].
    async fn docker(&self) -> Option<&SandboxExecutor> {
        self.docker
            .get_or_init(|| async {
                match DockerProvider::connect().await {
                    Ok(provider) => Some(self.sandbox_executor(Arc::new(provider))),
                    Err(error) => {
                        tracing::warn!(error = ?error, "no docker daemon for container scopes");
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    /// The name prefix every container this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        self.identity.container_prefix().await
    }

    /// The one-shot container prefix for the scope whose environment id is
    /// `instance`, for a leak check: `petri-<run id>-<instance>-s`.
    pub async fn one_shot_prefix_for(&self, instance: &str) -> Result<String, EnvError> {
        Ok(self
            .identity
            .one_shot_prefix(instance)
            .await?
            .as_str()
            .to_owned())
    }
}

#[async_trait]
impl Executor for RoutingExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        match scope.runtime.target {
            RuntimeTarget::HostProcess => {
                // Docker actions in a host job run as one-shot containers on
                // the local daemon; bind a runner to the host environment.
                let run_dir = self.identity.run_dir();
                let instance = scope.environment.as_str();
                let one_shots = self.identity.one_shot_prefix(instance).await?;
                let scope_dir = scope_dir(run_dir, instance);
                oneshot::sweep_scope(&one_shots, &scope_dir).await;
                let handle = self.host.acquire(scope, ctx).await?;
                let runner = OneShotRunner::new(
                    one_shots,
                    workspace_dir(run_dir, scope.workspace_id.as_str()),
                    &scope_dir,
                    scope,
                    None,
                    ctx,
                );
                Ok(handle.with_runner(Arc::new(runner)))
            }
            RuntimeTarget::Container { .. } => match self.docker().await {
                Some(docker) => docker.acquire(scope, ctx).await,
                None => Err(EnvError::backend(
                    "docker",
                    "acquire",
                    "this scope needs a container but no Docker daemon is configured",
                )),
            },
        }
    }

    /// Release goes back to the backend whose teardown record the handle
    /// carries: the handle itself says which acquired it.
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        if env.teardown::<SandboxTeardown>().is_some() {
            return match self.docker().await {
                Some(docker) => docker.release(env, outcome).await,
                None => ReleaseReport::default()
                    .problem("a docker environment has no docker executor to release it"),
            };
        }
        if env.teardown::<HostTeardown>().is_none() {
            return ReleaseReport::default().problem(
                "no backend of this router acquired this environment; nothing was released",
            );
        }
        // Sweep the host job's action containers and their env files before
        // releasing the process environment.
        match self.identity.one_shot_prefix(env.instance()).await {
            Ok(one_shots) => {
                let scope_dir = scope_dir(self.identity.run_dir(), env.instance());
                oneshot::sweep_scope(&one_shots, &scope_dir).await;
                self.host.release(env, outcome).await
            }
            Err(error) => self
                .host
                .release(env, outcome)
                .await
                .problem(format!("one-shot containers were not swept: {error}")),
        }
    }
}
