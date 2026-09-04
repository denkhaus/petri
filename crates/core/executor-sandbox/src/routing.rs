//! An [`Executor`] that routes each scope to the backend its runtime target
//! needs: a host process to the native [`HostExecutor`], a container to the
//! Docker [`SandboxExecutor`]. It is the composition the runtime registers for
//! a run.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::SandboxProvider;
use sandbox_driver_docker::DockerProvider;
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::oneshot::{self, ContainerPrefix, OneShotRunner};
use crate::{HostExecutor, SandboxExecutor, container_name, load_or_record_run_id};

/// Which backend acquired a scope, so release reaches the same one.
#[derive(Clone, Copy)]
enum Route {
    Host,
    Docker,
}

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
    run_dir:   PathBuf,
    retention: Retention,
    run_id:    OnceCell<SmolStr>,
    routes:    Mutex<HashMap<(ir::ScopeId, SmolStr), (Route, ContainerPrefix)>>,
}

impl RoutingExecutor {
    /// Builds a router. `docker_provider` is optional so a host-only run needs
    /// no daemon; both backends root their workspaces under `run_dir`.
    pub fn new(
        docker_provider: Option<Arc<dyn SandboxProvider>>,
        run_dir: PathBuf,
        retention: Retention,
    ) -> Self {
        let host = HostExecutor::new(run_dir.clone()).with_retention(retention);
        let docker = docker_provider.map(|provider| {
            SandboxExecutor::new(provider, run_dir.clone()).with_retention(retention)
        });
        Self {
            host,
            docker: OnceCell::new_with(Some(docker)),
            run_dir,
            retention,
            run_id: OnceCell::new(),
            routes: Mutex::new(HashMap::new()),
        }
    }

    /// A router over this machine: the native host executor, and the local
    /// Docker daemon connected on the first container scope. The standard
    /// runtime's default.
    pub fn local(run_dir: PathBuf, retention: Retention) -> Self {
        let host = HostExecutor::new(run_dir.clone()).with_retention(retention);
        Self {
            host,
            docker: OnceCell::new(),
            run_dir,
            retention,
            run_id: OnceCell::new(),
            routes: Mutex::new(HashMap::new()),
        }
    }

    /// The Docker executor, connecting to the local daemon on first use when
    /// the router was built with [`RoutingExecutor::local`].
    async fn docker(&self) -> Option<&SandboxExecutor> {
        self.docker
            .get_or_init(|| async {
                match DockerProvider::connect().await {
                    Ok(provider) => Some(
                        SandboxExecutor::new(Arc::new(provider), self.run_dir.clone())
                            .with_retention(self.retention),
                    ),
                    Err(error) => {
                        tracing::warn!(error = ?error, "no docker daemon for container scopes");
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    fn record(&self, scope: &ScopeSpec, route: Route, one_shots: ContainerPrefix) {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (scope.id, SmolStr::new(scope.environment.as_str())),
                (route, one_shots),
            );
    }

    fn take_route(&self, env: &EnvHandle) -> Option<(Route, ContainerPrefix)> {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(env.scope(), SmolStr::new(env.instance())))
    }

    /// The one-shot container prefix for a scope, from the run id and the
    /// scope's environment id — the same key on both routes, so a re-acquire
    /// and release sweep exactly this scope's action containers.
    async fn one_shot_prefix(&self, scope: &ScopeSpec) -> Result<ContainerPrefix, EnvError> {
        Ok(ContainerPrefix::new(
            self.one_shot_prefix_for(scope.environment.as_str()).await?,
        ))
    }

    /// The name prefix every container this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        let run_id = self.resolve_run_id().await?;
        Ok(format!("petri-{run_id}-"))
    }

    /// The one-shot container prefix for the scope whose environment id is
    /// `instance`, for a leak check: `petri-<run id>-<instance>-s`.
    pub async fn one_shot_prefix_for(&self, instance: &str) -> Result<String, EnvError> {
        let run_id = self.resolve_run_id().await?;
        Ok(format!(
            "{}-s",
            container_name(&format!("{run_id}/{instance}"))
        ))
    }

    async fn resolve_run_id(&self) -> Result<SmolStr, EnvError> {
        self.run_id
            .get_or_try_init(|| load_or_record_run_id(&self.run_dir))
            .await
            .cloned()
    }

    /// The env-file directory for a host scope's one-shot action containers.
    fn scope_dir(&self, scope: &ScopeSpec) -> PathBuf {
        self.run_dir
            .join("scopes")
            .join(scope.workspace_id.as_str())
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
                let one_shots = self.one_shot_prefix(scope).await?;
                oneshot::sweep(&one_shots).await;
                let handle = self.host.acquire(scope, ctx).await?;
                let scope_dir = self.scope_dir(scope);
                let runner = OneShotRunner::new(
                    one_shots.clone(),
                    self.host.workspace_for(scope.environment.as_str()),
                    scope_dir.join("exec-env"),
                    scope_dir.join("one-shots"),
                    scope,
                    None,
                    ctx,
                );
                self.record(scope, Route::Host, one_shots);
                Ok(handle.with_runner(Arc::new(runner)))
            }
            RuntimeTarget::Container { .. } => {
                let Some(docker) = self.docker().await else {
                    return Err(EnvError::Backend {
                        backend:   SmolStr::new("docker"),
                        operation: SmolStr::new("acquire"),
                        message:
                            "this scope needs a container but no Docker daemon is configured".into(),
                    });
                };
                let handle = docker.acquire(scope, ctx).await?;
                // The Docker executor bound the scope's one-shot runner; record
                // an empty prefix here since it sweeps its own.
                self.record(scope, Route::Docker, ContainerPrefix::new(String::new()));
                Ok(handle)
            }
        }
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let instance = SmolStr::new(env.instance());
        match self.take_route(&env) {
            Some((Route::Host, one_shots)) => {
                // Sweep the host job's action containers and their env files
                // before releasing the process environment.
                oneshot::sweep(&one_shots).await;
                let env_files = self
                    .run_dir
                    .join("scopes")
                    .join(instance.as_str())
                    .join("exec-env");
                let _ = fs::remove_dir_all(&env_files).await;
                self.host.release(env, outcome).await
            }
            Some((Route::Docker, _)) => match self.docker().await {
                Some(docker) => docker.release(env, outcome).await,
                None => ReleaseReport::default()
                    .problem("a docker route has no docker executor to release it"),
            },
            None => ReleaseReport::default()
                .problem("no backend is recorded for this scope; nothing was released"),
        }
    }
}
