//! The composed local executor: the full local experience over one run dir.
//!
//! The two pure executors stay directly usable — [`HostExecutor`] is
//! Docker-free, [`DockerExecutor`] serves containerized scopes — and this is
//! the composition that puts the Docker facilities behind *every* scope:
//! routing by [`RuntimeTarget`], one-shot container runners bound to host
//! scopes too (Docker actions always use the daemon, whichever executor owns
//! the env), and, for scopes that declare them, sidecar services.
//!
//! It lives here rather than in the `executor` crate because it must name every
//! implementation, and the interface crate names none — assembly is this
//! crate's job.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use executor_docker::DockerExecutor;
use executor_host::HostExecutor;
use ir::{RuntimeTarget, ScopeId};
use smol_str::SmolStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Host {
        /// The scope has a service world to tear down beside its processes.
        services: bool,
    },
    Container,
}

/// Routes each scope to the executor for its [`RuntimeTarget`] and binds the
/// shared Docker facilities to the result.
pub struct LocalExecutor {
    host:   HostExecutor,
    docker: DockerExecutor,
    /// Which executor acquired each live environment, so release goes back to
    /// it. Keyed by scope and instance name; one `LocalExecutor` serves one
    /// run directory, where instance names are unique.
    routes: Mutex<HashMap<(ScopeId, SmolStr), Which>>,
}

impl LocalExecutor {
    /// Both executors over one run dir, so a resumed run's executors reach the
    /// crashed run's environments.
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        let run_dir = run_dir.into();
        Self {
            host:   HostExecutor::new(&run_dir),
            docker: DockerExecutor::new(&run_dir),
            routes: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.host = self.host.with_retention(retention);
        self.docker = self.docker.with_retention(retention);
        self
    }

    /// The container-name prefix every container of this run shares, for leak
    /// checks.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        self.docker.container_prefix().await
    }

    fn record(&self, scope: &ScopeSpec, which: Which) {
        self.routes
            .lock()
            .expect("route table is not poisoned")
            .insert((scope.id, scope.instance.clone()), which);
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        match &scope.runtime.target {
            RuntimeTarget::Container { .. } => {
                let handle = self.docker.acquire(scope, ctx).await?;
                self.record(scope, Which::Container);
                Ok(handle)
            }
            RuntimeTarget::HostProcess => {
                // The fence covers this scope's one-shot containers and (when
                // it declares any) its service world too: a crashed run's
                // Docker side dies before its names can be reused. Best
                // effort, like the Docker fence — a daemon that is down has
                // nothing of ours to remove. The one-shot marker gates the
                // sweep, so a Docker-free scope never spawns a `docker` client
                // for a fence with nothing to look for.
                if self.docker.one_shots_marked(&scope.instance).await {
                    let prefix = self.docker.one_shot_prefix(&scope.instance).await?;
                    executor_docker::sweep_containers(&prefix).await;
                }
                let services = !scope.services.is_empty();
                if services {
                    self.docker.sweep_scope_services(&scope.instance).await?;
                }

                // Services first — steps must find them healthy — realized by
                // the Docker facilities; the host executor gets a spec with
                // none (it is deliberately Docker-free and would refuse).
                let network = self.docker.realize_host_services(scope, ctx).await?;
                let mut host_scope = scope.clone();
                host_scope.services = Vec::new();
                let handle = match self.host.acquire(&host_scope, ctx).await {
                    Ok(handle) => handle,
                    Err(error) => {
                        // A failed acquire leaks nothing.
                        if services {
                            let _ = self.docker.sweep_scope_services(&scope.instance).await;
                        }
                        return Err(error);
                    }
                };
                // Docker actions in a host job run against the daemon, never
                // through the job environment; the runner is bound here, at
                // acquisition — on the scope's network when it has one — and a
                // missing daemon surfaces when it is used.
                let runner = self.docker.scope_runner(scope, network, ctx).await?;
                self.record(scope, Which::Host { services });
                Ok(handle.with_runner(runner))
            }
        }
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let which = self
            .routes
            .lock()
            .expect("route table is not poisoned")
            .remove(&(env.scope(), SmolStr::new(env.instance())));
        match which {
            Some(Which::Container) => self.docker.release(env, outcome).await,
            Some(Which::Host { services }) => {
                // One-shot leftovers first — a step aborted at its hard
                // deadline can leave its container running — then the host's
                // own process groups and workspace, then the service world
                // (nothing of the scope uses it any more). The marker gates
                // the sweep, as at acquire: no one-shot ever launched means
                // nothing to look for.
                if self.docker.one_shots_marked(env.instance()).await
                    && let Ok(prefix) = self.docker.one_shot_prefix(env.instance()).await
                {
                    executor_docker::sweep_containers(&prefix).await;
                }
                let instance = env.instance().to_string();
                let mut report = self.host.release(env, outcome).await;
                if services {
                    report = match self.docker.sweep_scope_services(&instance).await {
                        Ok(()) => report.released(format!("services of `{instance}`")),
                        Err(e) => report.problem(format!("could not sweep services: {e}")),
                    };
                }
                report
            }
            None => ReleaseReport::default().problem(format!(
                "no executor is recorded for scope {} instance `{}`; nothing was released",
                env.scope(),
                env.instance()
            )),
        }
    }
}
