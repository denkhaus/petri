//! An [`Executor`] that routes each scope to the backend its runtime target
//! needs: a host process to the native [`HostExecutor`], a container to the
//! [`SandboxExecutor`] over the Docker plugin. It is the composition the
//! runtime registers for a run.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::SandboxId;
use tokio::sync::OnceCell;

use crate::actions::{self, ActionHostRunner};
use crate::host::HostTeardown;
use crate::lease::{LeaseLedger, MemoryLedger};
use crate::plugin::{FixedProvider, PluginError, PluginSettings, PluginSupervisor, ProviderSource};
use crate::run::{RunIdentity, workspace_dir};
use crate::{HostExecutor, SandboxExecutor, SandboxTeardown};

/// The provider kind container scopes go to.
pub const CONTAINER_KIND: &str = "docker";

/// What ends a host scope's action host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionOwner {
    Lease(SandboxLeaseId),
    Scope(ir::ScopeId),
}

/// How the container side is built: from this process's environment, or
/// from a provider a test handed in.
enum ContainerSource {
    Env { dev: Option<bool> },
    Fixed(Arc<dyn ProviderSource>),
}

/// Routes scopes to the native host executor or the container
/// [`SandboxExecutor`] by runtime target.
///
/// A host-process scope runs as real processes on this machine, with the
/// sentinel and crash fence a bare process needs; a container scope runs on
/// the Docker provider, reached through its plugin. The plugin is launched
/// on the first container scope, so a host-only run never touches a daemon,
/// and a container scope fails routably at acquire when it cannot be.
pub struct RoutingExecutor {
    host:         HostExecutor,
    source:       ContainerSource,
    container:    OnceCell<Result<Arc<SandboxExecutor>, String>>,
    /// The action hosts of host scopes, each keyed by what ends it: the
    /// lease that owns it, or the scope itself when no coordinator named one.
    action_hosts: Mutex<Vec<(ActionOwner, Arc<ActionHostRunner>)>>,
    ledger:       OnceLock<Arc<dyn LeaseLedger>>,
    identity:     Arc<RunIdentity>,
    retention:    Retention,
}

impl RoutingExecutor {
    /// A router over this machine: the native host executor, and the Docker
    /// plugin launched on the first container scope. The standard runtime's
    /// default.
    pub fn local(run_dir: impl Into<PathBuf>, retention: Retention) -> Self {
        Self::over(run_dir, retention, ContainerSource::Env { dev: None })
    }

    /// [`RoutingExecutor::local`] with the plugin dev-mode decision made by
    /// the caller (the CLI's `--sandbox-plugin-dev`).
    pub fn local_with_dev(run_dir: impl Into<PathBuf>, retention: Retention, dev: bool) -> Self {
        Self::over(run_dir, retention, ContainerSource::Env { dev: Some(dev) })
    }

    /// A router whose container scopes go to `source`: tests over a fake
    /// or fixed provider.
    pub fn with_provider_source(
        source: Arc<dyn ProviderSource>,
        run_dir: impl Into<PathBuf>,
        retention: Retention,
    ) -> Self {
        Self::over(run_dir, retention, ContainerSource::Fixed(source))
    }

    /// A router over a fixed in-process provider, for tests.
    pub fn with_provider(
        provider: Arc<dyn sandbox_driver::SandboxProvider>,
        run_dir: impl Into<PathBuf>,
        retention: Retention,
    ) -> Self {
        Self::with_provider_source(Arc::new(FixedProvider::new(provider)), run_dir, retention)
    }

    fn over(run_dir: impl Into<PathBuf>, retention: Retention, source: ContainerSource) -> Self {
        let run_dir = run_dir.into();
        Self {
            host: HostExecutor::new(run_dir.clone()).with_retention(retention),
            source,
            container: OnceCell::new(),
            action_hosts: Mutex::new(Vec::new()),
            ledger: OnceLock::new(),
            identity: Arc::new(RunIdentity::new(run_dir)),
            retention,
        }
    }

    /// The durable ledger container leases are recorded in. A coordinator
    /// sets it before the first container scope; without one, leases live
    /// in memory and sandboxes end with their scopes.
    pub fn set_ledger(&self, ledger: Arc<dyn LeaseLedger>) {
        let _ = self.ledger.set(ledger);
    }

    pub fn identity(&self) -> &Arc<RunIdentity> {
        &self.identity
    }

    fn provider_source(&self) -> Result<Arc<dyn ProviderSource>, PluginError> {
        match &self.source {
            ContainerSource::Fixed(source) => Ok(Arc::clone(source)),
            ContainerSource::Env { dev } => {
                let settings = PluginSettings::from_env(CONTAINER_KIND, *dev)?;
                Ok(Arc::new(PluginSupervisor::new(settings)))
            }
        }
    }

    fn host_address(&self, source: &dyn ProviderSource) -> Result<String, PluginError> {
        match &self.source {
            ContainerSource::Fixed(_) => Ok(crate::DOCKER_HOST_ALIAS.to_owned()),
            ContainerSource::Env { dev } => {
                let _ = source;
                PluginSettings::from_env(CONTAINER_KIND, *dev)?.host_address()
            }
        }
    }

    /// The container executor, built on first use. A configuration error
    /// is remembered: every container scope then fails routably with it.
    async fn container(&self) -> Result<Arc<SandboxExecutor>, EnvError> {
        self.container
            .get_or_init(|| async {
                let source = self.provider_source().map_err(|error| error.to_string())?;
                let host_address = self
                    .host_address(source.as_ref())
                    .map_err(|error| error.to_string())?;
                let ledger =
                    self.ledger.get().cloned().unwrap_or_else(|| {
                        Arc::new(MemoryLedger::default()) as Arc<dyn LeaseLedger>
                    });
                Ok(Arc::new(SandboxExecutor::new(
                    source,
                    ledger,
                    Arc::clone(&self.identity),
                    self.retention,
                    host_address,
                )))
            })
            .await
            .clone()
            .map_err(|message| EnvError::backend(CONTAINER_KIND, "acquire", message))
    }

    /// The name prefix every sandbox this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        self.identity.container_prefix().await
    }

    /// Ends a lease: stops its sandbox, then keeps or deletes it by this
    /// router's retention for `outcome`. The coordinator calls this when
    /// the invocation that owns the lease finishes.
    pub async fn release_lease(
        &self,
        lease: SandboxLeaseId,
        outcome: ScopeOutcome,
    ) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let action_hosts: Vec<Arc<ActionHostRunner>> = {
            let mut hosts = self
                .action_hosts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let (mine, rest): (Vec<_>, Vec<_>) = hosts
                .drain(..)
                .partition(|(owner, _)| *owner == ActionOwner::Lease(lease));
            *hosts = rest;
            mine.into_iter().map(|(_, runner)| runner).collect()
        };
        for runner in action_hosts {
            match runner.teardown().await {
                Ok(true) => report = report.released("action host"),
                Ok(false) => {}
                Err(error) => {
                    report = report.problem(format!("action host teardown failed: {error}"));
                }
            }
        }
        // The lease may be a container lease this process never acquired —
        // a resumed run whose invocation finished from replay — so the
        // container side is built here if it was not yet.
        match self.container().await {
            Ok(container) => {
                let ended = container
                    .manager()
                    .release_lease(lease, self.retention, outcome)
                    .await;
                report.released.extend(ended.released);
                report.kept.extend(ended.kept);
                report.problems.extend(ended.problems);
            }
            Err(error) => {
                report = report.problem(format!(
                    "no container executor to release lease {lease}: {error}"
                ));
            }
        }
        report
    }

    /// Deletes a recorded lease's sandbox with no live handle: prune.
    /// `workspace_id` is the lease's workspace, the reconcile key for a
    /// record that never learned its resource id.
    pub async fn delete_recorded(
        &self,
        lease: SandboxLeaseId,
        workspace_id: &str,
    ) -> Result<Vec<SandboxId>, EnvError> {
        self.container()
            .await?
            .manager()
            .delete_recorded(lease, workspace_id)
            .await
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
                // A crashed predecessor's action host, and its one-shots,
                // go before the scope is used again — only when a marker
                // says there was one, so a Docker-free run never launches
                // the plugin.
                if let Ok(source) = self.provider_source() {
                    actions::sweep_stale(
                        source.as_ref(),
                        &self.identity,
                        scope.workspace_id.as_str(),
                    )
                    .await;
                }
                let handle = self.host.acquire(scope, ctx).await?;
                // Docker actions in a host job run as one-shot containers
                // beside an action host on the local daemon, created when
                // the first one runs; a machine without a plugin still runs
                // the host scope, and the action then fails routably.
                match self.provider_source() {
                    Ok(source) => {
                        let host_address = self
                            .host_address(source.as_ref())
                            .unwrap_or_else(|_| crate::DOCKER_HOST_ALIAS.to_owned());
                        let runner = Arc::new(ActionHostRunner::new(
                            source,
                            Arc::clone(&self.identity),
                            workspace_dir(self.identity.run_dir(), scope.workspace_id.as_str()),
                            scope,
                            host_address,
                        ));
                        let owner = ctx
                            .lease()
                            .map_or(ActionOwner::Scope(scope.id), ActionOwner::Lease);
                        self.action_hosts
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push((owner, Arc::clone(&runner)));
                        Ok(handle.with_runner(runner))
                    }
                    Err(error) => {
                        tracing::debug!(error = %error, "no container plugin for host-scope actions");
                        Ok(handle)
                    }
                }
            }
            RuntimeTarget::Container { .. } => self.container().await?.acquire(scope, ctx).await,
        }
    }

    /// Release goes back to the backend whose teardown record the handle
    /// carries: the handle itself says which acquired it.
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        if env.teardown::<SandboxTeardown>().is_some() {
            return match self.container.get() {
                Some(Ok(container)) => container.release(env, outcome).await,
                _ => ReleaseReport::default()
                    .problem("a container environment has no container executor to release it"),
            };
        }
        if env.teardown::<HostTeardown>().is_none() {
            return ReleaseReport::default().problem(
                "no backend of this router acquired this environment; nothing was released",
            );
        }
        // A standalone host scope's action host goes with the scope; a
        // coordinator-owned one goes with its lease, at `release_lease`.
        let scope = env.scope();
        let mut report = self.host.release(env, outcome).await;
        let mine: Vec<Arc<ActionHostRunner>> = {
            let mut hosts = self
                .action_hosts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let (mine, rest): (Vec<_>, Vec<_>) = hosts
                .drain(..)
                .partition(|(owner, _)| *owner == ActionOwner::Scope(scope));
            *hosts = rest;
            mine.into_iter().map(|(_, runner)| runner).collect()
        };
        for runner in mine {
            match runner.teardown().await {
                Ok(true) => report = report.released("action host"),
                Ok(false) => {}
                Err(error) => {
                    report = report.problem(format!("action host teardown failed: {error}"));
                }
            }
        }
        report
    }
}
