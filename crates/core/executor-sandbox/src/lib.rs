//! Petri executors over the sandbox-driver provider family.
//!
//! Two shapes share the [`Executor`] interface. [`SandboxExecutor`] realizes a
//! container scope on a sandbox-driver provider reached over the JSON-RPC
//! plugin protocol — Docker today, Daytona next: the provider owns process
//! execution, the filesystem, one-shot containers, and teardown, and this
//! crate maps a [`ScopeSpec`] onto a `SandboxSpec` and keys the sandbox by
//! its durable lease. [`HostExecutor`] keeps the host backend native — real
//! processes in a workspace directory, with the process-group sentinel and
//! crash fence that a bare host process needs and a container does not.
//! [`RoutingExecutor`] sends each scope to the one its runtime target needs.
//!
//! No provider crate is linked here: every provider is a plugin process
//! ([`plugin`]), which is the path a third-party provider must take, so
//! Petri's own take it too.

mod actions;
mod env;
mod files;
mod host;
pub mod lease;
pub mod plugin;
mod routing;
mod run;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::{Sandbox, SandboxSource, SandboxSpec};
use sandbox_driver_docker_config::{DockerProviderConfig, Health, RegistryAuth, Sidecar};
use smol_str::SmolStr;

use crate::env::{OneShotRunner, SandboxEnv};
pub use crate::host::HostExecutor;
pub use crate::lease::{
    LeaseLedger, LeaseRecord, LeaseState, LedgerError, MemoryLedger, PendingIntent,
    SandboxLeaseManager,
};
pub use crate::plugin::{
    FixedProvider, PluginError, PluginSettings, PluginSupervisor, ProviderSource,
};
pub use crate::routing::{CONTAINER_KIND, RoutingExecutor};
pub use crate::run::{LEASE_LABEL, RUN_ID_FILE, RUN_LABEL, RunIdentity, WORKSPACE_LABEL};

/// The container path every scope's workspace lives at.
pub const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container on a local daemon reaches the driver's machine
/// through; the provider config maps it to the daemon's gateway.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";

/// The backend name this crate's errors carry.
const BACKEND: &str = "sandbox";

pub(crate) fn acquire_failed(error: &sandbox_driver::Error) -> EnvError {
    EnvError::backend(BACKEND, "acquire", error.to_string())
}

/// An [`Executor`] that realizes container scopes on one sandbox-driver
/// provider. The sandbox owns its workspace; every acquisition names the
/// lease it lives under, and one live handle serves every holder of that
/// lease.
pub struct SandboxExecutor {
    manager:      Arc<SandboxLeaseManager>,
    identity:     Arc<RunIdentity>,
    retention:    Retention,
    host_address: String,
}

impl SandboxExecutor {
    /// Builds an executor over `source`, with `ledger` as the durable
    /// record of its leases; `identity` names the run.
    pub fn new(
        source: Arc<dyn ProviderSource>,
        ledger: Arc<dyn LeaseLedger>,
        identity: Arc<RunIdentity>,
        retention: Retention,
        host_address: String,
    ) -> Self {
        Self {
            manager: Arc::new(SandboxLeaseManager::new(source, ledger, identity.clone())),
            identity,
            retention,
            host_address,
        }
    }

    pub fn manager(&self) -> &Arc<SandboxLeaseManager> {
        &self.manager
    }

    /// The name prefix every sandbox this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        self.identity.container_prefix().await
    }

    async fn acquire_inner(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        // A coordinator names the lease; a bare driver has none, and its
        // sandbox is the scope's alone, keyed by the scope and ended with it.
        let (lease, standalone) = match ctx.lease() {
            Some(lease) => (lease, false),
            None => (SandboxLeaseId::new(u64::from(scope.id.raw())), true),
        };
        let name = self.identity.container_name(lease).await?;
        let sandbox = self
            .manager
            .acquire(
                lease::LeaseRequest {
                    lease,
                    workspace_id: scope.workspace_id.as_str(),
                },
                |labels| build_spec(scope, labels, &name, ctx),
            )
            .await?;

        // The ambient environment is a fact the steps rely on (`PATH` for
        // the process step, say); a provider that cannot report it is not
        // one this executor can serve, and says so at acquire.
        let ambient = sandbox.environment().await.map_err(|error| {
            EnvError::backend(
                BACKEND,
                "acquire",
                format!("the sandbox's environment could not be read: {error}"),
            )
        })?;
        let workspace = sandbox.working_directory().to_owned();
        let env = SandboxEnv {
            sandbox: sandbox.clone(),
            workspace: workspace.clone(),
            ambient,
            grace: scope.grace,
            host_address: self.host_address.clone(),
        };
        // Docker actions in this scope run as one-shot containers in the
        // sandbox's world: same workspace, same services, same host alias.
        let runner = OneShotRunner {
            sandbox: sandbox.clone(),
            workspace,
            host_address: self.host_address.clone(),
            env: scope.env.clone(),
        };
        let teardown = SandboxTeardown {
            sandbox,
            lease,
            standalone,
        };
        Ok(EnvHandle::new(
            scope.id,
            SmolStr::new(scope.environment.as_str()),
            Arc::new(env),
            teardown,
        )
        .with_runner(Arc::new(runner)))
    }
}

/// Maps a container scope to a `SandboxSpec`: the workspace inside the
/// sandbox, the run's labels, and the provider's typed options.
fn build_spec(
    scope: &ScopeSpec,
    labels: &[(String, String)],
    name: &str,
    ctx: &AcquireContext,
) -> Result<SandboxSpec, EnvError> {
    let RuntimeTarget::Container {
        image,
        options,
        credentials,
    } = &scope.runtime.target
    else {
        return Err(EnvError::backend(
            BACKEND,
            "acquire",
            "this executor realizes container scopes only; a host-process scope needs the host \
             executor or a runner image",
        ));
    };
    let registry_auth = registry_auth(credentials.as_ref(), ctx)?;
    let sidecars = sidecars(scope, ctx)?;
    let provider_config = docker_provider_config(registry_auth, sidecars, options);
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: image.to_string(),
    })
    .working_directory(CONTAINER_WORKSPACE)
    .provider_config(provider_config.into_value())
    .name(name);
    for (key, value) in labels {
        spec = spec.label(key.clone(), value.clone());
    }
    spec.user = options.user.as_ref().map(ToString::to_string);

    // Scope env is the trusted channel for the container; a `-e` option lands
    // on top, as the later `docker create` flag would have.
    for (key, value) in &scope.env {
        spec = spec.env_var(key.as_str(), value.as_str());
    }
    for (key, value) in &options.env {
        spec = spec.env_var(key.as_str(), value.as_str());
    }
    Ok(spec)
}

/// Resolves image pull credentials to registry auth, or `None` when the
/// scope declares none.
fn registry_auth(
    credentials: Option<&ir::RegistryCredentials>,
    ctx: &AcquireContext,
) -> Result<Option<RegistryAuth>, EnvError> {
    let Some(credentials) = credentials else {
        return Ok(None);
    };
    let password = resolve_secret(&credentials.password_secret, ctx)?;
    Ok(Some(RegistryAuth {
        username: credentials.username.to_string(),
        password,
        server: None,
    }))
}

/// Maps the scope's services to Docker sidecars. Ports are dropped: a
/// containerized job reaches a service by its network alias, not a published
/// port.
fn sidecars(scope: &ScopeSpec, ctx: &AcquireContext) -> Result<Vec<Sidecar>, EnvError> {
    let mut sidecars = Vec::with_capacity(scope.services.len());
    for service in &scope.services {
        let typed = &service.options;
        let mut sidecar = Sidecar::new(service.name.as_str(), service.image.as_str());
        // Declared env first, then `-e` flags on top, as `docker create`
        // would apply them.
        sidecar.env = service
            .env
            .iter()
            .chain(typed.env.iter().map(|(key, value)| (key, value)))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        sidecar.dns = typed.dns.iter().map(ToString::to_string).collect();
        sidecar.cap_add = typed.cap_add.iter().map(ToString::to_string).collect();
        sidecar.user = typed.user.as_ref().map(ToString::to_string);
        sidecar.privileged = typed.privileged;
        sidecar.entrypoint = typed
            .entrypoint
            .as_ref()
            .map(|words| words.iter().map(ToString::to_string).collect());
        sidecar.health = typed.health.as_ref().map(|health| Health {
            cmd:             health
                .cmd
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            interval_ms:     health.interval_ms,
            timeout_ms:      health.timeout_ms,
            retries:         health.retries,
            start_period_ms: health.start_period_ms,
        });
        if let Some(credentials) = &service.credentials {
            let password = resolve_secret(&credentials.password_secret, ctx)?;
            sidecar.registry_auth = Some(RegistryAuth {
                username: credentials.username.to_string(),
                password,
                server: None,
            });
        }
        sidecars.push(sidecar);
    }
    Ok(sidecars)
}

/// Resolves a secret by name inside acquire, registering it for masking.
fn resolve_secret(name: &str, ctx: &AcquireContext) -> Result<String, EnvError> {
    ctx.secrets()
        .resolve(name)
        .map(|secret| secret.expose().to_string())
        .map_err(|error| {
            EnvError::backend(
                BACKEND,
                "acquire",
                format!("resolving secret `{name}`: {error}"),
            )
        })
}

/// The Docker `provider_config` for a scope container: an init process
/// reaps zombies, the host alias resolves to the gateway, and the
/// workspace is the sandbox's own volume — no bind from this machine.
fn docker_provider_config(
    registry_auth: Option<RegistryAuth>,
    sidecars: Vec<Sidecar>,
    container: &ir::ContainerOptions,
) -> DockerProviderConfig {
    // Steps run as a program plus arguments, so an image without bash
    // (alpine) works: the provider's exec wrapper needs only /bin/sh.
    DockerProviderConfig {
        init: true,
        privileged: container.privileged,
        platform: container.platform.as_ref().map(ToString::to_string),
        extra_hosts: vec![format!("{DOCKER_HOST_ALIAS}:host-gateway")],
        dns: container.dns.iter().map(ToString::to_string).collect(),
        cap_add: container.cap_add.iter().map(ToString::to_string).collect(),
        registry_auth,
        sidecars,
        ..DockerProviderConfig::default()
    }
}

/// What release needs: the lease the environment held, and whether this
/// executor alone owns it.
pub(crate) struct SandboxTeardown {
    sandbox:    Arc<dyn Sandbox>,
    lease:      SandboxLeaseId,
    /// No coordinator named the lease: the sandbox ends with the scope.
    standalone: bool,
}

impl fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("sandbox", &self.sandbox.id())
            .field("lease", &self.lease)
            .field("standalone", &self.standalone)
            .finish()
    }
}

#[async_trait]
impl Executor for SandboxExecutor {
    #[tracing::instrument(name = "scope.acquire", skip_all, fields(scope = scope.id.raw()))]
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        match self.acquire_inner(scope, ctx).await {
            Ok(handle) => {
                tracing::info!("scope environment acquired");
                Ok(handle)
            }
            Err(error) => {
                tracing::error!(error = ?error, "scope environment acquire failed");
                Err(error)
            }
        }
    }

    /// Drops this execution's holder. A standalone acquisition — no
    /// coordinator, no lease — ends its sandbox here by retention; a
    /// managed one leaves that to the lease's release.
    #[tracing::instrument(
        name = "scope.release",
        skip_all,
        fields(scope = env.scope().raw(), outcome = ?outcome)
    )]
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let Some(teardown) = env.teardown::<SandboxTeardown>() else {
            return ReleaseReport::default()
                .problem("sandbox executor was handed a foreign environment");
        };
        let (lease, standalone) = (teardown.lease, teardown.standalone);
        drop(env);
        let remaining = self.manager.release_holder(lease).await;
        let report = ReleaseReport::default().released(format!("holder of lease {lease}"));
        if !standalone || remaining > 0 {
            return report;
        }
        let ended = self
            .manager
            .release_lease(lease, self.retention, outcome)
            .await;
        ReleaseReport {
            released: report.released.into_iter().chain(ended.released).collect(),
            kept:     ended.kept,
            problems: ended.problems,
        }
    }
}
