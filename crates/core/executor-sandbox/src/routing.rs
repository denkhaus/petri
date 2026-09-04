//! An [`Executor`] that routes each scope to the backend its runtime target
//! needs: a host process to the host provider, a container to the Docker
//! provider. It is the composition the runtime registers for a run.

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
use smol_str::SmolStr;

use crate::{BackendKind, SandboxExecutor};

/// Which backend acquired a scope, so release reaches the same one.
#[derive(Clone, Copy)]
enum Route {
    Host,
    Docker,
}

/// Routes scopes to a host or Docker [`SandboxExecutor`] by runtime target.
///
/// A host-process scope runs on the host provider; a container scope runs on
/// the Docker provider. A run with no Docker daemon still serves its
/// host-process scopes, and a container scope then fails routably at acquire.
pub struct RoutingExecutor {
    host:   SandboxExecutor,
    docker: Option<SandboxExecutor>,
    routes: Mutex<HashMap<(ir::ScopeId, SmolStr), Route>>,
}

impl RoutingExecutor {
    /// Builds a router over the two providers. `docker` is optional so a
    /// host-only run needs no daemon.
    pub fn new(
        host_provider: Arc<dyn SandboxProvider>,
        docker_provider: Option<Arc<dyn SandboxProvider>>,
        run_dir: PathBuf,
        retention: Retention,
    ) -> Self {
        let host = SandboxExecutor::new(host_provider, BackendKind::Host, run_dir.clone())
            .with_retention(retention);
        let docker = docker_provider.map(|provider| {
            SandboxExecutor::new(provider, BackendKind::Docker, run_dir).with_retention(retention)
        });
        Self {
            host,
            docker,
            routes: Mutex::new(HashMap::new()),
        }
    }

    fn record(&self, scope: &ScopeSpec, route: Route) {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((scope.id, SmolStr::new(scope.environment.as_str())), route);
    }

    fn take_route(&self, env: &EnvHandle) -> Option<Route> {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(env.scope(), SmolStr::new(env.instance())))
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
                let handle = self.host.acquire(scope, ctx).await?;
                self.record(scope, Route::Host);
                Ok(handle)
            }
            RuntimeTarget::Container { .. } => {
                let Some(docker) = &self.docker else {
                    return Err(EnvError::Backend {
                        backend:   SmolStr::new("docker"),
                        operation: SmolStr::new("acquire"),
                        message:
                            "this scope needs a container but no Docker daemon is configured".into(),
                    });
                };
                let handle = docker.acquire(scope, ctx).await?;
                self.record(scope, Route::Docker);
                Ok(handle)
            }
        }
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        match self.take_route(&env) {
            Some(Route::Host) => self.host.release(env, outcome).await,
            Some(Route::Docker) => match &self.docker {
                Some(docker) => docker.release(env, outcome).await,
                None => ReleaseReport::default()
                    .problem("a docker route has no docker executor to release it"),
            },
            None => ReleaseReport::default()
                .problem("no backend is recorded for this scope; nothing was released"),
        }
    }
}
