//! Petri executors over the sandbox-driver provider family.
//!
//! Two shapes share the [`Executor`] interface. [`SandboxExecutor`] realizes a
//! container scope on a sandbox-driver provider — Docker today, Daytona later:
//! the provider owns process execution, the filesystem, and teardown, and this
//! crate maps a [`ScopeSpec`] onto a `SandboxSpec`. [`HostExecutor`] keeps the
//! host backend native — real processes in a workspace directory, with the
//! process-group sentinel and crash fence that a bare host process needs and a
//! container does not. [`RoutingExecutor`] sends each scope to the one its
//! runtime target needs.

mod env;
mod files;
mod host;
mod oneshot;
mod routing;
mod run;

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::{fmt, mem};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::{Sandbox, SandboxFilter, SandboxProvider, SandboxSource, SandboxSpec};
use sandbox_driver_docker::{BindMount, DockerProviderConfig, Health, RegistryAuth, Sidecar};
use smol_str::SmolStr;
use tokio::fs;

use crate::env::SandboxEnv;
pub use crate::host::HostExecutor;
use crate::oneshot::OneShotRunner;
pub use crate::oneshot::list_containers;
pub use crate::routing::RoutingExecutor;
pub use crate::run::RUN_ID_FILE;
use crate::run::{RunIdentity, scope_dir, workspace_dir};

/// The container path every scope's workspace is mounted at.
const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container reaches the driver's machine through.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";
/// The label naming a sandbox's environment, for the reconcile fence. Its
/// value is unique per run directory and scope, so `acquire` finds and ends
/// exactly the crashed predecessors of this environment.
pub const ENVIRONMENT_LABEL: &str = "petri.environment";

/// The backend name this crate's errors carry.
const BACKEND: &str = "sandbox";

/// An [`Executor`] that realizes container scopes on one sandbox-driver
/// provider. The workspace is a host directory bind-mounted into the
/// container, so logs, artifacts, and retention see it on the host.
pub struct SandboxExecutor {
    provider:  Arc<dyn SandboxProvider>,
    identity:  Arc<RunIdentity>,
    retention: Retention,
}

impl SandboxExecutor {
    /// Builds an executor over `provider`; `run_dir` roots every workspace
    /// under `scopes/`.
    pub fn new(provider: Arc<dyn SandboxProvider>, run_dir: impl Into<PathBuf>) -> Self {
        Self::with_identity(
            provider,
            Arc::new(RunIdentity::new(run_dir.into())),
            Retention::default(),
        )
    }

    /// An executor sharing its run identity with the other executors over the
    /// same run dir, so every name they derive agrees.
    pub(crate) fn with_identity(
        provider: Arc<dyn SandboxProvider>,
        identity: Arc<RunIdentity>,
        retention: Retention,
    ) -> Self {
        Self {
            provider,
            identity,
            retention,
        }
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// The name prefix every container this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release. Reads or mints the
    /// run id, so it is stable across executors over one run dir.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        self.identity.container_prefix().await
    }

    async fn acquire_inner(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let run_dir = self.identity.run_dir();
        let instance = scope.environment.as_str();
        let workspace_host = workspace_dir(run_dir, scope.workspace_id.as_str());
        let scope_dir = scope_dir(run_dir, instance);

        // The environment label is the reconcile key: a matching sandbox is a
        // crashed predecessor, ended before the fresh create. One-shot action
        // containers hang off the job container; their prefix is a fence
        // target too, so a crash leaves nothing behind. The three touch
        // nothing in common, so they run together.
        let label = self.identity.environment_label(instance).await?;
        let job_name = self.identity.container_name(instance).await?;
        let one_shot_prefix = self.identity.one_shot_prefix(instance).await?;
        let (created, fenced, ()) = tokio::join!(
            fs::create_dir_all(&workspace_host),
            self.fence_by_label(&label),
            oneshot::sweep_scope(&one_shot_prefix, &scope_dir),
        );
        created.map_err(|error| EnvError::workspace("create", workspace_host.display(), error))?;
        fenced?;

        let spec = build_spec(
            scope,
            &workspace_host.display().to_string(),
            &label,
            &job_name,
            ctx,
        )?;
        let sandbox = self.create_guarded(spec).await?;

        let ambient = sandbox.environment().await.unwrap_or_default();
        let env = SandboxEnv {
            sandbox: sandbox.clone(),
            workspace_host: workspace_host.clone(),
            ambient,
            grace: scope.grace,
        };

        // Docker actions in this scope run as one-shot containers on the job's
        // network namespace, sharing its services and host alias.
        let runner = OneShotRunner::new(
            one_shot_prefix.clone(),
            workspace_host.clone(),
            &scope_dir,
            scope,
            Some(format!("container:{job_name}")),
            ctx,
        );
        let teardown = SandboxTeardown {
            sandbox,
            retention: self.retention,
            workspace_host,
            one_shot_prefix,
            scope_dir,
        };
        Ok(
            EnvHandle::new(scope.id, SmolStr::new(instance), Arc::new(env), teardown)
                .with_runner(Arc::new(runner)),
        )
    }

    /// Creates the sandbox in a task this future's drop does not abort, and
    /// removes it when nobody waited for it. Dropping an in-flight create does
    /// not cancel the daemon's create: the container appears moments after the
    /// client is gone, past any fence that already ran — Created, never
    /// started, cleaned by nothing. The guard closes that leak: whichever side
    /// sees the other's mark deletes the sandbox, exactly once.
    async fn create_guarded(&self, spec: SandboxSpec) -> Result<Arc<dyn Sandbox>, EnvError> {
        let slot: Arc<Mutex<AbandonSlot>> = Arc::new(Mutex::new(AbandonSlot::default()));
        let guard = AbandonGuard { slot: slot.clone() };
        let provider = self.provider.clone();
        let task_slot = slot.clone();
        let created = tokio::spawn(async move {
            let sandbox = provider.create(&spec, None).await?;
            let abandoned = {
                let mut slot = task_slot.lock().unwrap_or_else(PoisonError::into_inner);
                if slot.abandoned {
                    true
                } else {
                    slot.sandbox = Some(sandbox.clone());
                    false
                }
            };
            if abandoned {
                let _ = sandbox.delete().await;
            }
            Ok::<_, sandbox_driver::Error>(sandbox)
        });
        let sandbox = created
            .await
            .map_err(|error| {
                EnvError::backend(
                    BACKEND,
                    "acquire",
                    format!("the create task failed: {error}"),
                )
            })?
            .map_err(|error| acquire_failed(&error))?;
        guard.defuse();
        Ok(sandbox)
    }

    /// Ends every sandbox that carries `label`: a crashed predecessor of this
    /// environment. Removing it makes its status unreadable and frees the
    /// deterministic name for the fresh create. Deletion is by id, with no
    /// handle: a predecessor that is stopped or half-created goes the same
    /// way as a running one.
    async fn fence_by_label(&self, label: &str) -> Result<(), EnvError> {
        let mut filter = SandboxFilter::default();
        filter
            .labels
            .insert(ENVIRONMENT_LABEL.to_owned(), label.to_owned());
        let matches = self
            .provider
            .list(&filter)
            .await
            .map_err(|error| acquire_failed(&error))?;
        for status in matches {
            if let Err(error) = self.provider.delete(&status.id, None).await {
                tracing::warn!(error = ?error, "fencing a prior sandbox failed");
            }
        }
        Ok(())
    }
}

/// Maps a container scope to a `SandboxSpec`, pinning the deterministic
/// `name` and the environment `label` so a re-acquire targets the same one.
fn build_spec(
    scope: &ScopeSpec,
    workspace_host: &str,
    label: &str,
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
    let provider_config = docker_provider_config(workspace_host, registry_auth, sidecars, options);
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: image.to_string(),
    })
    .working_directory(CONTAINER_WORKSPACE)
    .provider_config(provider_config.into_value())
    .name(name)
    .label(ENVIRONMENT_LABEL, label);
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

fn acquire_failed(error: &sandbox_driver::Error) -> EnvError {
    EnvError::backend(BACKEND, "acquire", error.to_string())
}

/// The Docker `provider_config` for a scope container: the workspace is a
/// host bind mount, an init process reaps zombies, and the host alias
/// resolves to the gateway.
fn docker_provider_config(
    workspace_host: &str,
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
        binds: vec![BindMount {
            host:      workspace_host.to_owned(),
            container: CONTAINER_WORKSPACE.to_owned(),
            mode:      None,
        }],
        extra_hosts: vec![format!("{DOCKER_HOST_ALIAS}:host-gateway")],
        dns: container.dns.iter().map(ToString::to_string).collect(),
        cap_add: container.cap_add.iter().map(ToString::to_string).collect(),
        registry_auth,
        sidecars,
        ..DockerProviderConfig::default()
    }
}

/// What release needs: the sandbox to tear down, whether to keep a failed
/// workspace, where that workspace lives on the host, and the scope's
/// one-shot world to sweep.
pub(crate) struct SandboxTeardown {
    sandbox:         Arc<dyn Sandbox>,
    retention:       Retention,
    workspace_host:  PathBuf,
    one_shot_prefix: oneshot::ContainerPrefix,
    /// Holds the one-shot marker and the per-spawn env files, which are never
    /// retained: they can hold resolved secrets.
    scope_dir:       PathBuf,
}

impl fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("sandbox", &self.sandbox.id())
            .field("retention", &self.retention)
            .field("workspace_host", &self.workspace_host)
            .field("one_shot_prefix", &self.one_shot_prefix.as_str())
            .field("scope_dir", &self.scope_dir)
            .finish()
    }
}

/// Shared between an acquire future and its create task: the created sandbox,
/// and whether the acquire was dropped before it took delivery.
#[derive(Default)]
struct AbandonSlot {
    abandoned: bool,
    sandbox:   Option<Arc<dyn Sandbox>>,
}

/// Marks the acquire abandoned on drop, unless defused. If the create task has
/// already delivered a sandbox nobody will release, the drop removes it.
struct AbandonGuard {
    slot: Arc<Mutex<AbandonSlot>>,
}

impl AbandonGuard {
    fn defuse(self) {
        mem::forget(self);
    }
}

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        let orphan = {
            let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            slot.abandoned = true;
            slot.sandbox.take()
        };
        if let Some(sandbox) = orphan {
            // Best effort, like the rest of the fence: on a runtime that is
            // itself shutting down the spawn is dropped unrun.
            let _detached = tokio::spawn(async move {
                let _ = sandbox.delete().await;
            });
        }
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

    #[tracing::instrument(
        name = "scope.release",
        skip_all,
        fields(scope = env.scope().raw(), outcome = ?outcome)
    )]
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let Some(teardown) = env.teardown::<SandboxTeardown>() else {
            return report.problem("sandbox executor was handed a foreign environment");
        };
        let sandbox = teardown.sandbox.clone();
        let retention = teardown.retention;
        let workspace_host = teardown.workspace_host.clone();
        let one_shot_prefix = teardown.one_shot_prefix.clone();
        let scope_dir = teardown.scope_dir.clone();
        drop(env);

        // One-shot action containers may hang off the job container's netns;
        // remove them first, and their env files whatever the retention policy.
        oneshot::sweep_scope(&one_shot_prefix, &scope_dir).await;

        // The workspace is a host bind mount, so the container holds nothing
        // worth keeping: delete it always.
        match sandbox.delete().await {
            Ok(()) => report = report.released(format!("container {}", sandbox.id())),
            Err(error) => {
                tracing::warn!(error = ?error, "sandbox delete failed");
                report = report.problem(format!("sandbox delete failed: {error}"));
            }
        }

        // Keep the host workspace on a failure the retention policy keeps.
        let workspace = format!("workspace {}", workspace_host.display());
        if retention.keeps(outcome) {
            return report.kept(workspace);
        }
        match fs::remove_dir_all(&workspace_host).await {
            Ok(()) => report.released(workspace),
            Err(error) if error.kind() == ErrorKind::NotFound => report.released(workspace),
            Err(error) => {
                tracing::warn!(error = ?error, "workspace removal failed");
                report.problem(format!(
                    "could not remove {}: {error}",
                    workspace_host.display()
                ))
            }
        }
    }
}
