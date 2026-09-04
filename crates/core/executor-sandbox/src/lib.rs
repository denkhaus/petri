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
mod host;
mod oneshot;
mod options;
mod routing;

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::{fmt, mem, process};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome,
    ScopeSpec,
};
use ir::RuntimeTarget;
use sandbox_driver::{Sandbox, SandboxFilter, SandboxProvider, SandboxSource, SandboxSpec};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::OnceCell;

use crate::env::SandboxEnv;
pub use crate::host::HostExecutor;
use crate::oneshot::{ContainerPrefix, OneShotRunner};
pub use crate::routing::RoutingExecutor;

/// Every container on the local daemon whose name starts with `prefix`. For
/// tests and sweeps that check a run left nothing behind; best-effort, empty
/// when no daemon answers.
pub async fn list_containers(prefix: &str) -> Vec<String> {
    oneshot::list_containers(prefix).await
}

/// The container path every scope's workspace is mounted at.
const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container reaches the driver's machine through.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";
/// The label naming a sandbox's environment, for the reconcile fence. Its
/// value is unique per run directory and scope, so `acquire` finds and ends
/// exactly the crashed predecessors of this environment.
const ENVIRONMENT_LABEL: &str = "petri.environment";
/// The file under the run dir holding the run id the environment label
/// carries; minted once, read back by any executor over the same run dir.
pub const RUN_ID_FILE: &str = "sandbox-run-id";

/// An [`Executor`] that realizes container scopes on one sandbox-driver
/// provider. The workspace is a host directory bind-mounted into the
/// container, so logs, artifacts, and retention see it on the host.
pub struct SandboxExecutor {
    provider:  Arc<dyn SandboxProvider>,
    run_dir:   PathBuf,
    retention: Retention,
    /// The run id the environment label carries, resolved on first use from
    /// the run dir so a resuming executor reaches the crashed run's sandboxes.
    run_id:    OnceCell<SmolStr>,
}

impl SandboxExecutor {
    /// Builds an executor over `provider`; `run_dir` roots every workspace
    /// under `scopes/`.
    pub fn new(provider: Arc<dyn SandboxProvider>, run_dir: PathBuf) -> Self {
        Self {
            provider,
            run_dir,
            retention: Retention::default(),
            run_id: OnceCell::new(),
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
        let run_id = self.resolve_run_id().await?;
        Ok(format!("petri-{run_id}-"))
    }

    /// The host directory a scope's workspace lives in: the bind-mount source
    /// for the container.
    fn workspace_host_path(&self, workspace_id: &str) -> PathBuf {
        self.run_dir.join("scopes").join(workspace_id).join("work")
    }

    async fn acquire_inner(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let workspace_host = self.workspace_host_path(scope.workspace_id.as_str());
        fs::create_dir_all(&workspace_host)
            .await
            .map_err(|error| EnvError::workspace("create", workspace_host.display(), error))?;
        let workspace_host_str = workspace_host.display().to_string();

        // The environment label is the reconcile key: a matching sandbox is a
        // crashed predecessor, ended before the fresh create.
        let label = self.environment_label(scope).await?;
        self.fence_by_label(&label).await?;

        // One-shot action containers hang off the job container; their prefix
        // is a fence target too, so a crash leaves nothing behind.
        let job_name = container_name(&label);
        let one_shot_prefix = ContainerPrefix::new(format!("{job_name}-s"));
        oneshot::sweep(&one_shot_prefix).await;

        let spec = build_spec(scope, &workspace_host_str, &label, ctx)?;
        let sandbox = self.create_guarded(spec).await?;

        let ambient = sandbox.environment().await.unwrap_or_default();
        let env = SandboxEnv::new(
            sandbox.clone(),
            CONTAINER_WORKSPACE.to_owned(),
            workspace_host.clone(),
            DOCKER_HOST_ALIAS.to_owned(),
            ambient,
            scope.grace,
        );

        // Docker actions in this scope run as one-shot containers on the job's
        // network namespace, sharing its services and host alias.
        let scope_dir = self
            .run_dir
            .join("scopes")
            .join(scope.workspace_id.as_str());
        let env_files = scope_dir.join("exec-env");
        let runner = OneShotRunner::new(
            one_shot_prefix.clone(),
            workspace_host.clone(),
            env_files.clone(),
            scope_dir.join("one-shots"),
            scope,
            Some(format!("container:{job_name}")),
            ctx,
        );
        let teardown = SandboxTeardown {
            sandbox,
            retention: self.retention,
            workspace_host: workspace_host.clone(),
            one_shot_prefix,
            env_files,
        };
        Ok(EnvHandle::new(
            scope.id,
            SmolStr::new(scope.environment.as_str()),
            Arc::new(env),
            teardown,
        )
        .with_runner(Arc::new(runner)))
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
            .map_err(|error| EnvError::Backend {
                backend:   SmolStr::new("sandbox"),
                operation: SmolStr::new("acquire"),
                message:   format!("the create task failed: {error}"),
            })?
            .map_err(|error| acquire_failed(&error))?;
        guard.defuse();
        Ok(sandbox)
    }

    /// The environment label value for a scope: the run id and the scope's
    /// environment id, globally unique because the run id is minted once per
    /// run directory.
    async fn environment_label(&self, scope: &ScopeSpec) -> Result<String, EnvError> {
        let run_id = self.resolve_run_id().await?;
        Ok(format!("{run_id}/{}", scope.environment.as_str()))
    }

    async fn resolve_run_id(&self) -> Result<SmolStr, EnvError> {
        self.run_id
            .get_or_try_init(|| load_or_record_run_id(&self.run_dir))
            .await
            .cloned()
    }

    /// Ends every sandbox that carries `label`: a crashed predecessor of this
    /// environment. Removing it makes its status unreadable and frees the
    /// deterministic name for the fresh create.
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
            match self.provider.attach(&status.id, None).await {
                Ok(sandbox) => {
                    if let Err(error) = sandbox.delete().await {
                        tracing::warn!(error = ?error, "fencing a prior sandbox failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "attaching a prior sandbox for the fence failed");
                }
            }
        }
        Ok(())
    }
}

/// Maps a container scope to a `SandboxSpec`, pinning a deterministic name and
/// the environment `label` so a re-acquire targets the same one.
fn build_spec(
    scope: &ScopeSpec,
    workspace_host: &str,
    label: &str,
    ctx: &AcquireContext,
) -> Result<SandboxSpec, EnvError> {
    let RuntimeTarget::Container {
        image,
        options,
        credentials,
    } = &scope.runtime.target
    else {
        return Err(EnvError::Backend {
            backend:   SmolStr::new("sandbox"),
            operation: SmolStr::new("acquire"),
            message:   "this executor realizes container scopes only; a host-process scope \
                        needs the host executor or a runner image"
                .into(),
        });
    };
    let container = options::parse_container(options)?;
    let registry_auth = registry_auth(credentials.as_ref(), ctx)?;
    let sidecars = sidecars(scope, ctx)?;
    let provider_config =
        docker_provider_config(workspace_host, registry_auth, sidecars, &container);
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: image.to_string(),
    })
    .working_directory(CONTAINER_WORKSPACE)
    .provider_config(provider_config)
    .name(container_name(label))
    .label(ENVIRONMENT_LABEL, label);
    spec.user.clone_from(&container.user);

    // Scope env is the trusted channel for the container; a `-e` option lands
    // on top, as the later `docker create` flag would have.
    for (key, value) in &scope.env {
        spec = spec.env_var(key.as_str(), value.as_str());
    }
    for (key, value) in &container.env {
        spec = spec.env_var(key.as_str(), value.as_str());
    }
    Ok(spec)
}

/// Resolves image pull credentials to a `registry_auth` value, or `None` when
/// the scope declares none.
fn registry_auth(
    credentials: Option<&ir::RegistryCredentials>,
    ctx: &AcquireContext,
) -> Result<Option<serde_json::Value>, EnvError> {
    let Some(credentials) = credentials else {
        return Ok(None);
    };
    let password = resolve_secret(&credentials.password_secret, ctx)?;
    Ok(Some(serde_json::json!({
        "username": credentials.username.as_str(),
        "password": password,
    })))
}

/// Maps the scope's services to Docker sidecars. Ports are dropped: a
/// containerized job reaches a service by its network alias, not a published
/// port.
fn sidecars(scope: &ScopeSpec, ctx: &AcquireContext) -> Result<Vec<serde_json::Value>, EnvError> {
    let mut sidecars = Vec::with_capacity(scope.services.len());
    for service in &scope.services {
        let env: serde_json::Map<String, serde_json::Value> = service
            .env
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str().to_owned(),
                    serde_json::Value::String(value.to_string()),
                )
            })
            .collect();
        let parsed = options::parse_service(&service.options)?;
        let mut env = env;
        for (key, value) in &parsed.env {
            env.insert(key.clone(), serde_json::Value::String(value.clone()));
        }
        let mut sidecar = serde_json::json!({
            "name": service.name.as_str(),
            "image": service.image.as_str(),
            "env": env,
            "dns": parsed.dns,
            "cap_add": parsed.cap_add,
        });
        if let Some(user) = &parsed.user {
            sidecar["user"] = serde_json::Value::String(user.clone());
        }
        if let Some(entrypoint) = &parsed.entrypoint {
            sidecar["entrypoint"] = serde_json::json!(entrypoint);
        }
        if let Some(health) = &parsed.health {
            let mut check = serde_json::json!({
                "cmd": health.cmd.clone().unwrap_or_default(),
            });
            if let Some(ms) = health.interval_ms {
                check["interval_ms"] = serde_json::json!(ms);
            }
            if let Some(ms) = health.timeout_ms {
                check["timeout_ms"] = serde_json::json!(ms);
            }
            if let Some(retries) = health.retries {
                check["retries"] = serde_json::json!(retries);
            }
            if let Some(ms) = health.start_period_ms {
                check["start_period_ms"] = serde_json::json!(ms);
            }
            sidecar["health"] = check;
        }
        if let Some(credentials) = &service.credentials {
            let password = resolve_secret(&credentials.password_secret, ctx)?;
            sidecar["registry_auth"] = serde_json::json!({
                "username": credentials.username.as_str(),
                "password": password,
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
        .map_err(|error| EnvError::Backend {
            backend:   SmolStr::new("sandbox"),
            operation: SmolStr::new("acquire"),
            message:   format!("resolving secret `{name}`: {error}"),
        })
}

fn acquire_failed(error: &sandbox_driver::Error) -> EnvError {
    EnvError::Backend {
        backend:   SmolStr::new("sandbox"),
        operation: SmolStr::new("acquire"),
        message:   error.to_string(),
    }
}

/// A deterministic sandbox name from an environment label, so a re-acquire
/// targets the same container. Non-name characters become hyphens.
pub(crate) fn container_name(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("petri-{sanitized}")
}

/// Reads the run id from the run dir, minting and recording it on first use.
/// Write-then-rename, so a reader sees the whole id or none.
pub(crate) async fn load_or_record_run_id(run_dir: &PathBuf) -> Result<SmolStr, EnvError> {
    let path = run_dir.join(RUN_ID_FILE);
    let io_error =
        |action, path: &Path, error: io::Error| EnvError::workspace(action, path.display(), error);
    match fs::read_to_string(&path).await {
        Ok(recorded) if !recorded.trim().is_empty() => {
            return Ok(SmolStr::new(recorded.trim()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("read", &path, error)),
    }
    let minted = fresh_run_id();
    fs::create_dir_all(run_dir)
        .await
        .map_err(|error| io_error("create", run_dir, error))?;
    let staged = run_dir.join(format!("{RUN_ID_FILE}.tmp"));
    fs::write(&staged, minted.as_bytes())
        .await
        .map_err(|error| io_error("write", &staged, error))?;
    fs::rename(&staged, &path)
        .await
        .map_err(|error| io_error("rename", &path, error))?;
    Ok(SmolStr::new(minted))
}

/// Unique across processes and time.
fn fresh_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{nanos:x}-{}-{}",
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The Docker `provider_config` for a scope container: the workspace is a
/// host bind mount, an init process reaps zombies, and the host alias
/// resolves to the gateway.
fn docker_provider_config(
    workspace_host: &str,
    registry_auth: Option<serde_json::Value>,
    sidecars: Vec<serde_json::Value>,
    container: &options::ContainerOptions,
) -> serde_json::Value {
    // `shell: auto`: steps run through `exec 'prog' 'args'`, which needs no
    // Bash semantics, so an image without bash (alpine) still works — the
    // provider probes for /bin/bash and falls back to /bin/sh.
    let mut config = serde_json::json!({
        "init": true,
        "shell": "auto",
        "binds": [{ "host": workspace_host, "container": CONTAINER_WORKSPACE }],
        "extra_hosts": [format!("{DOCKER_HOST_ALIAS}:host-gateway")],
        "dns": container.dns,
        "cap_add": container.cap_add,
        "privileged": container.privileged,
    });
    if let Some(platform) = &container.platform {
        config["platform"] = serde_json::Value::String(platform.clone());
    }
    if let Some(auth) = registry_auth {
        config["registry_auth"] = auth;
    }
    if !sidecars.is_empty() {
        config["sidecars"] = serde_json::Value::Array(sidecars);
    }
    config
}

/// What release needs: the sandbox to tear down, whether to keep a failed
/// workspace, and where that workspace lives on the host.
struct SandboxTeardown {
    sandbox:         Arc<dyn Sandbox>,
    retention:       Retention,
    workspace_host:  PathBuf,
    one_shot_prefix: ContainerPrefix,
    /// Per-spawn env files: never retained, they can hold resolved secrets.
    env_files:       PathBuf,
}

impl fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("sandbox", &self.sandbox.id())
            .field("retention", &self.retention)
            .field("workspace_host", &self.workspace_host)
            .field("one_shot_prefix", &self.one_shot_prefix.as_str())
            .field("env_files", &self.env_files)
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
        let env_files = teardown.env_files.clone();
        drop(env);

        // One-shot action containers may hang off the job container's netns;
        // remove them first, and their env files whatever the retention policy.
        oneshot::sweep(&one_shot_prefix).await;
        let _ = fs::remove_dir_all(&env_files).await;

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
