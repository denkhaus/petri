//! The Docker executor: a long-lived container with the workspace bind-mounted,
//! and steps run through `docker exec`.
//!
//! Implements the [`executor`] interface. The bind mount is what keeps the two
//! sides symmetric: [`ExecEnv::workspace`] is the host path, so artifacts and
//! logs are handled the same way whether or not a container is involved.
//!
//! # Why `docker exec … kill`, and not `docker kill`
//!
//! `docker kill` signals PID 1 of the container. Steps are not PID 1 — they are
//! `docker exec` processes, siblings of init — so `docker kill` never reaches
//! them. Cancelling a step is therefore `docker exec <c> kill -<SIG> --
//! -<PGID>`, run inside the container against the step's own process group.
//! Container-level kill is reserved for scope release, where killing everything
//! is the point.
//!
//! To have a process group to signal, each step is launched under `setsid` and
//! records its own pid into a file on the bind-mounted workspace, where the
//! host can read it. `setsid` makes the shell a session leader, so its pid *is*
//! its pgid. **The image must provide `setsid`** (busybox and util-linux both
//! do).
//!
//! # Why `setsid` runs under a keeper shell, and the exit status comes from a file
//!
//! `setsid` forks when its caller is already a process group leader, and
//! `docker exec` does hand its process one (runc gives it a group of its own).
//! Run directly, `setsid` therefore exits as soon as the child is running and
//! the client returns 0 immediately — the step is still going, its real exit
//! status is lost, and, worse, the client has *detached*: every line the step
//! prints from then on is lost too. Fast steps never show it; a step that
//! pauses for a download loses everything after the pause.
//!
//! So the step runs under a keeper: the process `docker exec` attaches to is a
//! plain shell that runs `setsid` as its child. A child is not a group leader,
//! so `setsid` never forks — it makes the session and execs the wrapper — and
//! the keeper stays attached until the wrapper exits, forwarding output and
//! status.
//!
//! The wrapper still records the status in a file, and [`DockerProcess::wait`]
//! prefers that file over the client's exit code, polling the process group's
//! liveness rather than trusting the client: correct even if something kills
//! the keeper out from under a step, instead of correct only when everything
//! behaves.

mod oneshot;
mod services;

use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, mem};

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    AcquireContext, ContainerRunner, EnvError, EnvHandle, ExecEnv, Executor, ExitStatus,
    LineStream, ProcessHandle, ProcessSpec, ReleaseReport, Retention, ScopeOutcome, ScopeSpec, Sig,
};
use ir::RuntimeTarget;
use smol_str::SmolStr;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, mpsc};
use tokio::time::{self, Instant};
use tracing::field::Empty;

use crate::oneshot::OneShotRunner;

/// Where the workspace is mounted inside the container.
pub const CONTAINER_WORKSPACE: &str = "/workspace";

/// How a process inside any container this executor creates reaches the
/// driver's machine. Docker Desktop resolves the alias by itself; on a Linux
/// daemon it exists because every create passes
/// `--add-host=host.docker.internal:host-gateway` — the executor guarantees
/// the name, so `host_address` can promise it.
pub(crate) const HOST_ALIAS: &str = "host.docker.internal";

/// The `--add-host` argument that backs [`HOST_ALIAS`] on every daemon.
pub(crate) const ADD_HOST_GATEWAY: &str = "--add-host=host.docker.internal:host-gateway";

/// The run id's file under the run dir. Container names carry the id and the
/// acquire fence is remove-by-name, so the id must outlive the process that
/// minted it: it is recorded here before the run's first container exists, and
/// any later executor over the same run dir — a resuming process's — reads it
/// back and reaches the same containers. A fork into a fresh run dir gets a
/// fresh id.
pub const RUN_ID_FILE: &str = "docker-run-id";

/// How often [`DockerProcess::wait`] checks whether the step's process group is
/// still alive, once the `docker exec` client has returned without a recorded
/// status.
///
/// This sets two things at once: how long after a step really ends before the
/// driver notices, and how much idle `docker exec` traffic a long step costs.
/// At 50ms a ten-minute step costs about 12,000 liveness checks in the worst
/// case — the case where `setsid` forked. When it did not fork, which is the
/// common case, the status file is already there on the first look and the loop
/// never runs.
pub const LIVENESS_POLL: Duration = Duration::from_millis(50);

/// How long to wait for the step to record its pgid before giving up on
/// signalling the group. A cancel can arrive before the step has written it.
pub(crate) const PGID_WAIT: Duration = Duration::from_secs(2);

/// When to pull an image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PullPolicy {
    #[default]
    IfNotPresent,
    Always,
    Never,
}

/// Runs steps inside containers.
pub struct DockerExecutor {
    run_dir:   PathBuf,
    /// Resolved on first use from [`RUN_ID_FILE`]: the run dir's recorded id,
    /// or a fresh one recorded there.
    run_id:    OnceCell<SmolStr>,
    retention: Retention,
    pull:      PullPolicy,
}

impl DockerExecutor {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir:   run_dir.into(),
            run_id:    OnceCell::new(),
            retention: Retention::default(),
            pull:      PullPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    #[must_use]
    pub fn with_pull_policy(mut self, pull: PullPolicy) -> Self {
        self.pull = pull;
        self
    }

    /// Whether a Docker daemon is reachable. Tests skip rather than fail
    /// without one.
    pub async fn is_available() -> bool {
        run_docker(&["info", "--format", "{{.ServerVersion}}"])
            .await
            .is_ok()
    }

    /// The run id, recorded in the run dir the first time anything needs it.
    async fn run_id(&self) -> Result<&SmolStr, EnvError> {
        self.run_id
            .get_or_try_init(|| load_or_record_run_id(&self.run_dir))
            .await
    }

    /// The name prefix every container of this run shares, for leak checks.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        Ok(format!("petri-{}-", self.run_id().await?))
    }

    async fn container_name(&self, instance: &str) -> Result<String, EnvError> {
        let sanitized: String = instance
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Ok(format!("{}{sanitized}", self.container_prefix().await?))
    }

    /// The workspace directory a scope instance gets, shared with the host
    /// executor's layout so the composed executor can bind either.
    fn workspace_for(&self, instance: &str) -> PathBuf {
        self.run_dir.join("scopes").join(instance).join("work")
    }

    /// The container-name prefix of a scope instance's one-shot containers, for
    /// fencing and leak checks.
    pub async fn one_shot_prefix(&self, instance: &str) -> Result<String, EnvError> {
        Ok(one_shot_prefix_of(&self.container_name(instance).await?))
    }

    /// Where a scope instance's one-shot marker lives: recorded before the
    /// runner's first launch, so a later process over the same run dir knows
    /// whether the instance may have one-shot leftovers to fence away.
    fn one_shot_marker(&self, instance: &str) -> PathBuf {
        self.run_dir.join("scopes").join(instance).join("one-shots")
    }

    /// Whether an acquisition over this run dir ever launched one-shot
    /// containers for `instance`. A `false` means the fence and release sweep
    /// have nothing to look for — no daemon roundtrip needed.
    pub async fn has_one_shot_marker(&self, instance: &str) -> bool {
        fs::try_exists(self.one_shot_marker(instance))
            .await
            .unwrap_or(false)
    }

    /// Realize `scope`'s sidecar services for an environment acquired
    /// elsewhere — a host scope under the composed local executor. Ports
    /// publish to the host, since the scope's steps reach services over
    /// localhost. Returns the scope network for the runner to attach to,
    /// `None` when the scope declares no services.
    pub async fn realize_host_services(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<Option<String>, EnvError> {
        let base = self.container_name(&scope.instance).await?;
        services::realize(scope, &base, true, self.pull, ctx).await
    }

    /// Tear down (or fence away) a host scope's service world: its containers
    /// by prefix, then its network. Best effort and idempotent.
    pub async fn sweep_scope_services(&self, instance: &str) -> Result<(), EnvError> {
        let base = self.container_name(instance).await?;
        services::sweep(&base).await;
        Ok(())
    }

    /// A one-shot runner bound to a scope this executor did *not* acquire — a
    /// host scope under the composed local executor. Same run identity, image
    /// cache and cleanup fence as this executor's own scopes. `network` is the
    /// scope's network when it has one; `None` runs on the daemon default.
    ///
    /// Cheap and daemon-free: a missing daemon surfaces when the runner is
    /// used, so a Docker-free run never fails for a runner it never touches.
    pub async fn scope_runner(
        &self,
        scope: &ScopeSpec,
        network: Option<String>,
        ctx: &AcquireContext,
    ) -> Result<Arc<dyn ContainerRunner>, EnvError> {
        Ok(Arc::new(OneShotRunner::new(
            self.one_shot_prefix(&scope.instance).await?,
            self.workspace_for(&scope.instance),
            self.one_shot_marker(&scope.instance),
            scope,
            network,
            self.pull,
            ctx,
        )))
    }
}

/// The one-shot container-name prefix hanging off a scope container's name.
/// One spelling for the fence, the runner, and the release sweep.
fn one_shot_prefix_of(container: &str) -> String {
    format!("{container}-s")
}

/// Whether the policy calls for a pull now. The presence probe runs only when
/// the policy reads the answer.
async fn should_pull(image: &str, pull: PullPolicy) -> bool {
    match pull {
        PullPolicy::Never => false,
        PullPolicy::Always => true,
        PullPolicy::IfNotPresent => run_docker(&["image", "inspect", image]).await.is_err(),
    }
}

fn announce_pull(progress: &Arc<dyn executor::ProgressSink>, scope: ir::ScopeId, image: &str) {
    progress.progress(scope, executor::Progress::PullingImage {
        image: SmolStr::new(image),
    });
}

/// Make `image` available under the pull policy, announcing the pull when one
/// happens.
pub(crate) async fn prepare_registry_image(
    image: &str,
    pull: PullPolicy,
    scope: ir::ScopeId,
    progress: &Arc<dyn executor::ProgressSink>,
) -> Result<(), EnvError> {
    if should_pull(image, pull).await {
        announce_pull(progress, scope, image);
        pull_image(&[], image).await?;
    }
    Ok(())
}

/// The one fallback platform: GitHub-hosted runners are linux/amd64, so that
/// is what CI images target — an image with no manifest for this daemon's
/// architecture is retried as amd64 and runs emulated. (The later `create`
/// warns about the mismatch and proceeds; the daemon needs emulation
/// configured, which Docker Desktop ships.)
const FALLBACK_PLATFORM: &str = "linux/amd64";

/// Whether a failed pull says the image exists but not for this daemon's
/// architecture — the error an arm64 host gets for an amd64-only CI image.
fn is_missing_platform(error: &EnvError) -> bool {
    matches!(error, EnvError::Backend { message, .. } if message.contains("no matching manifest"))
}

/// `docker pull` with `config_args` in front (a credentialed pull's isolated
/// `--config`), retrying an architecture miss as [`FALLBACK_PLATFORM`]. When
/// the retry fails too, the *first* error is the one reported — it names the
/// real problem, the missing architecture.
pub(crate) async fn pull_image(config_args: &[&str], image: &str) -> Result<(), EnvError> {
    let mut args: Vec<&str> = config_args.to_vec();
    args.extend(["pull", image]);
    let Err(error) = run_docker(&args).await else {
        return Ok(());
    };
    if !is_missing_platform(&error) {
        return Err(error);
    }
    let mut args: Vec<&str> = config_args.to_vec();
    args.extend(["pull", "--platform", FALLBACK_PLATFORM, image]);
    match run_docker(&args).await {
        Ok(_) => {
            // Every container from this image now runs under emulation, which
            // is otherwise an order-of-magnitude slowdown with no trace at all.
            tracing::warn!(
                image = %image,
                platform = FALLBACK_PLATFORM,
                "image pulled for a fallback platform"
            );
            Ok(())
        }
        Err(_) => Err(error),
    }
}

/// Make `image` available under the pull policy, with or without registry
/// credentials at the exact effect boundary.
pub(crate) async fn prepare_image(
    image: &str,
    credentials: Option<&ir::RegistryCredentials>,
    pull: PullPolicy,
    scope: ir::ScopeId,
    ctx: &AcquireContext,
) -> Result<(), EnvError> {
    let Some(credentials) = credentials else {
        return prepare_registry_image(image, pull, scope, ctx.progress()).await;
    };
    if !should_pull(image, pull).await {
        return Ok(());
    }
    announce_pull(ctx.progress(), scope, image);
    pull_with_credentials(image, credentials, ctx).await
}

/// Pull `image` as `credentials` — an isolated Docker config dir (the user's
/// own login is never touched), a login whose password travels stdin only, the
/// pull, and the config dir removed whatever happened. The password resolves
/// here, at the point of use; resolving registers it with the run's masker.
async fn pull_with_credentials(
    image: &str,
    credentials: &ir::RegistryCredentials,
    ctx: &AcquireContext,
) -> Result<(), EnvError> {
    let password = ctx
        .secrets()
        .resolve(&credentials.password_secret)
        .map_err(|e| EnvError::Backend {
            backend:   SmolStr::new("docker"),
            operation: SmolStr::new("login"),
            message:   e.to_string(),
        })?;
    let config_dir = env::temp_dir().join(format!(
        "petri-docker-login-{}-{}",
        process::id(),
        next_token()
    ));
    fs::create_dir_all(&config_dir)
        .await
        .map_err(|e| EnvError::workspace("create", config_dir.display(), e))?;
    let config = config_dir.display().to_string();
    let mut login: Vec<&str> = vec![
        "--config",
        &config,
        "login",
        "--username",
        credentials.username.as_str(),
        "--password-stdin",
    ];
    if let Some(host) = registry_host(image) {
        login.push(host);
    }
    let result = match run_docker_stdin("login", &login, password.expose().as_bytes()).await {
        Ok(_) => pull_image(&["--config", &config], image).await,
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(&config_dir).await;
    result
}

/// The registry host of an image reference, when it names one — the first
/// component when it looks like a host (a dot, a port, or `localhost`);
/// otherwise Docker Hub, which `docker login` takes with no server argument.
fn registry_host(image: &str) -> Option<&str> {
    let first = image.split('/').next()?;
    let is_host = first.contains('.') || first.contains(':') || first == "localhost";
    (image.contains('/') && is_host).then_some(first)
}

/// Remove every container whose name starts with `prefix`. Best effort, like
/// the rest of the fence: a daemon that is down has nothing of ours to remove.
pub async fn sweep_containers(prefix: &str) {
    for name in list_containers(prefix).await {
        let _ = run_docker(&["rm", "-f", "-v", &name]).await;
    }
}

/// Cleanup for an acquire nobody waited out. Release tears down what acquire
/// returned; this covers what acquire had already realized when its future was
/// dropped instead — the sweep abandoning a run that ignored its cancel is the
/// live case. Dropping while armed removes the scope's world by name, best
/// effort, on a detached task; the `abandoned` flag additionally reaches the
/// create task, which owns the window where the container's create is still in
/// flight at the daemon and remove-by-name has nothing to see yet.
struct AbandonGuard {
    container: String,
    services:  bool,
    abandoned: Arc<AtomicBool>,
    armed:     bool,
}

impl AbandonGuard {
    fn arm(container: &str, services: bool) -> Self {
        Self {
            container: container.to_string(),
            services,
            abandoned: Arc::new(AtomicBool::new(false)),
            armed: true,
        }
    }

    fn abandoned(&self) -> Arc<AtomicBool> {
        self.abandoned.clone()
    }

    /// Acquire returned: cleanup is inline on the error paths and the handle's
    /// on success.
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.abandoned.store(true, Ordering::SeqCst);
        let container = mem::take(&mut self.container);
        let services = self.services;
        // Detached on purpose: there is no future left to await from. On a
        // runtime that is itself shutting down the spawn is dropped unrun —
        // best effort, like the rest of the fence.
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                let _ = run_docker(&["rm", "-f", "-v", &container]).await;
                if services {
                    services::sweep(&container).await;
                }
            });
        }
    }
}

/// What release needs: the container to stop, the workspace, and whether to
/// keep it.
#[derive(Clone, Debug)]
struct DockerTeardown {
    container: String,
    path:      PathBuf,
    retention: Retention,
    grace:     Duration,
    /// The scope has a service world (containers and a network) to tear down.
    services:  bool,
}

/// The acquire-failure record. `EnvError::Backend`'s message is docker's own
/// stderr — for a service it carries the container's log tail — so the event
/// takes the variant and the structural fields the error already has, never the
/// message.
fn report_acquire_failure(error: &EnvError) {
    // A `None` field records nothing, so the two structural fields appear on the
    // `Backend` arm and are absent everywhere else.
    let (backend, operation) = match error {
        EnvError::Backend {
            backend, operation, ..
        } => (Some(backend.as_str()), Some(operation.as_str())),
        _ => (None, None),
    };
    tracing::error!(
        error_kind = error.kind(),
        backend,
        operation,
        "scope environment acquire failed"
    );
}

impl DockerExecutor {
    /// The whole of [`Executor::acquire`], so its several `?` sites report
    /// through one boundary.
    async fn acquire_inner(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let RuntimeTarget::Container {
            image,
            options,
            credentials,
        } = &scope.runtime.target
        else {
            return Err(EnvError::Backend {
                backend:   SmolStr::new("docker"),
                operation: SmolStr::new("acquire"),
                message:   "this scope does not ask for a container".into(),
            });
        };

        let workspace = self.workspace_for(&scope.instance);
        fs::create_dir_all(&workspace)
            .await
            .map_err(|e| EnvError::workspace("create", workspace.display(), e))?;

        let span = tracing::Span::current();
        span.record("image", image.as_str());
        prepare_image(image, credentials.as_ref(), self.pull, scope.id, ctx).await?;

        let name = self.container_name(&scope.instance).await?;
        span.record("container", name.as_str());
        // The fence half of the acquire contract (§9): a previous acquisition —
        // a crashed driver's included — left containers under these
        // deterministic names; removing them ends whatever still runs inside and
        // makes their status unreadable. The names carry the run id recorded in
        // the run dir, so a resuming process over the same run dir reaches the
        // crashed run's containers. One-shot containers and the service world a
        // crash left behind share the scope's prefixes and go the same way.
        let _ = run_docker(&["rm", "-f", &name]).await;
        sweep_containers(&one_shot_prefix_of(&name)).await;
        if !scope.services.is_empty() {
            services::sweep(&name).await;
        }

        // Armed for the rest of acquire: a caller that stops waiting drops
        // this future — an aborted driver task, a host's timeout — and
        // whatever the daemon has already realized for this scope must not
        // outlive the drop. Defused on every return, where cleanup is either
        // done inline or owned by the handle.
        let guard = AbandonGuard::arm(&name, !scope.services.is_empty());

        // The scope's services first: the job container joins their network at
        // creation, and every service is healthy before any step can fire.
        let network = match services::realize(scope, &name, false, self.pull, ctx).await {
            Ok(network) => network,
            Err(error) => {
                // realize failed and cleaned up after itself.
                guard.defuse();
                return Err(error);
            }
        };

        let mount = format!("{}:{CONTAINER_WORKSPACE}", workspace.display());
        let mut create: Vec<String> = vec![
            "create".into(),
            "--init".into(),
            "--name".into(),
            name.clone(),
            "-v".into(),
            mount,
            "-w".into(),
            CONTAINER_WORKSPACE.into(),
            ADD_HOST_GATEWAY.into(),
        ];
        if let Some(network) = &network {
            create.push("--network".into());
            create.push(network.clone());
        }
        for (key, value) in &scope.env {
            create.push("-e".into());
            create.push(format!("{key}={value}"));
        }
        // The scope's raw engine flags, passed through as declared.
        create.extend(options.iter().map(ToString::to_string));
        create.push(image.to_string());
        // A long-lived init command, so the container outlives any one step.
        create.extend(["sleep".to_string(), "infinity".to_string()]);

        // The create sequence runs in its own task, which a dropped acquire
        // does not abort. Killing the `docker create` client mid-request does
        // not cancel the daemon's create: the container appears moments
        // *after* the client is dead, past any remove-by-name sweep that
        // already ran — the leak this closes. The task always sees its own
        // requests through, then checks whether anyone is still waiting.
        let abandoned = guard.abandoned();
        let sequence = tokio::spawn({
            let name = name.clone();
            async move {
                let refs: Vec<&str> = create.iter().map(String::as_str).collect();
                let result = async {
                    run_docker(&refs).await?;
                    run_docker(&["start", &name]).await?;
                    // The environment a `docker exec` will start from,
                    // snapshotted once: the image's `Config.Env` with the `-e`
                    // flags above folded in — the fact behind
                    // [`ExecEnv::ambient_env`].
                    run_docker(&["inspect", "--format", "{{json .Config.Env}}", &name]).await
                }
                .await;
                if abandoned.load(Ordering::SeqCst) {
                    let _ = run_docker(&["rm", "-f", "-v", &name]).await;
                }
                result
            }
        });
        let created = match sequence.await {
            Ok(result) => result,
            Err(join) => Err(EnvError::Backend {
                backend:   SmolStr::new("docker"),
                operation: SmolStr::new("create"),
                message:   join.to_string(),
            }),
        };
        let ambient = match created.and_then(|env_json| parse_env_list(&env_json)) {
            Ok(ambient) => ambient,
            Err(error) => {
                // A failed acquire leaks nothing: the services came up for a job
                // container that will never exist.
                let _ = run_docker(&["rm", "-f", &name]).await;
                if network.is_some() {
                    services::sweep(&name).await;
                }
                guard.defuse();
                return Err(error);
            }
        };

        // One-shot containers in this scope's world share the job container's
        // network namespace, so a service reachable from the job is reachable
        // from them under the same names.
        let runner = OneShotRunner::new(
            one_shot_prefix_of(&name),
            workspace.clone(),
            self.one_shot_marker(&scope.instance),
            scope,
            Some(format!("container:{name}")),
            self.pull,
            ctx,
        );

        guard.defuse();
        Ok(EnvHandle::new(
            scope.id,
            scope.instance.clone(),
            Arc::new(DockerEnv {
                container: name.clone(),
                workspace: workspace.clone(),
                ambient,
                grace: scope.grace,
                wrapper_shell: OnceCell::new(),
            }),
            DockerTeardown {
                container: name,
                path:      workspace,
                retention: self.retention,
                grace:     scope.grace,
                services:  network.is_some(),
            },
        )
        .with_runner(Arc::new(runner)))
    }
}

#[async_trait]
impl Executor for DockerExecutor {
    #[tracing::instrument(
        name = "scope.acquire",
        skip_all,
        fields(
            scope = scope.id.raw(),
            instance = %scope.instance,
            image = Empty,
            service_count = scope.services.len(),
            container = Empty,
        )
    )]
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
                report_acquire_failure(&error);
                Err(error)
            }
        }
    }

    #[tracing::instrument(
        name = "scope.release",
        skip_all,
        fields(
            scope = env.scope().raw(),
            instance = %env.instance(),
            outcome = ?outcome,
            container = Empty,
        )
    )]
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let Some(DockerTeardown {
            container,
            path,
            retention,
            grace,
            services,
        }) = env.teardown::<DockerTeardown>()
        else {
            return report.problem("docker executor was handed a foreign environment");
        };
        tracing::Span::current().record("container", container.as_str());

        // One-shot containers first: they may hang off the job container's
        // network namespace, and everything of the scope is meant to stop.
        sweep_containers(&one_shot_prefix_of(container)).await;

        // Container-level kill is exactly right here: everything inside is meant to
        // stop. `stop` sends TERM and waits, then `rm -f` guarantees no leak.
        let grace_secs = grace.as_secs().max(1).to_string();
        let _ = run_docker(&["stop", "-t", &grace_secs, container]).await;
        match run_docker(&["rm", "-f", "-v", container]).await {
            Ok(_) => report = report.released(format!("container {container}")),
            Err(e) => {
                // A container left on the daemon. `e` is a `Backend` carrying
                // docker's stderr, so only the structural half is reported.
                tracing::warn!(
                    container = %container,
                    operation = "rm",
                    "container removal failed"
                );
                report = report.problem(format!("could not remove {container}: {e}"));
            }
        }

        // The service world goes with the scope — after the job container,
        // which was attached to its network.
        if *services {
            services::sweep(container).await;
            report = report.released(format!("services of {container}"));
        }

        let workspace = format!("workspace {}", path.display());
        if retention.keeps(outcome) {
            return report.kept(workspace);
        }
        match fs::remove_dir_all(path).await {
            Ok(()) => report = report.released(workspace),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                report = report.released(workspace);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = ?e, "workspace removal failed");
                report = report.problem(format!("could not remove {}: {e}", path.display()));
            }
        }
        report
    }
}

struct DockerEnv {
    container:     String,
    workspace:     PathBuf,
    /// The container's effective env, snapshotted at create.
    ambient:       BTreeMap<String, String>,
    grace:         Duration,
    /// The shell the pgid wrapper runs under, probed once per container:
    /// `bash` when the image has it, `sh` otherwise. Not a style choice — a
    /// POSIX `sh` that is dash or busybox *filters out* environment names that
    /// are not valid identifiers when it spawns children, and GitHub's
    /// contract passes exactly such names (`INPUT_INCLUDE-HIDDEN-FILES`); bash
    /// passes them through. The wrapper script is plain POSIX either way.
    wrapper_shell: OnceCell<&'static str>,
}

impl DockerEnv {
    async fn wrapper_shell(&self) -> &'static str {
        *self
            .wrapper_shell
            .get_or_init(|| async {
                let probe = run_docker(&[
                    "exec",
                    &self.container,
                    "sh",
                    "-c",
                    "command -v bash >/dev/null 2>&1",
                ])
                .await;
                if probe.is_ok() { "bash" } else { "sh" }
            })
            .await
    }
}

#[async_trait]
impl ExecEnv for DockerEnv {
    #[tracing::instrument(
        name = "process.spawn",
        level = "debug",
        skip_all,
        fields(
            container = %self.container,
            program = %spec.program,
            arg_count = spec.args.len(),
            env_count = spec.env.len(),
        )
    )]
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        // The pgid file lives on the bind mount, so the container writes it and the
        // host reads it.
        let token = format!("{}-{}", process::id(), next_token());
        let pgid_dir = self.workspace.join(".ci").join("pg");
        fs::create_dir_all(&pgid_dir)
            .await
            .map_err(|e| EnvError::workspace("create", pgid_dir.display(), e))?;
        let pgid_host = pgid_dir.join(&token);
        let pgid_in_container = format!("{CONTAINER_WORKSPACE}/.ci/pg/{token}");

        // Create the working directory through the bind mount, the parity of
        // the host executor's `create_dir_all`: `docker exec -w` refuses a
        // directory that does not exist yet (`repo/` before the first checkout).
        let workdir = match &spec.cwd {
            Some(rel) => {
                let host = self.workspace.join(rel);
                fs::create_dir_all(&host)
                    .await
                    .map_err(|e| EnvError::workspace("create", host.display(), e))?;
                format!("{CONTAINER_WORKSPACE}/{}", rel.display())
            }
            None => CONTAINER_WORKSPACE.to_string(),
        };

        let mut argv: Vec<String> = vec!["exec".into(), "-w".into(), workdir];
        for (key, value) in &spec.env {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
        }
        argv.push(self.container.clone());
        let wrapper_shell = self.wrapper_shell().await;
        argv.extend([
            // The keeper: the process `docker exec` attaches to. runc hands it a
            // process group of its own, so `setsid` run *directly* here forks and
            // exits at once — and the client detaches from a step still running,
            // losing every line it prints from then on. As the keeper's child,
            // `setsid` is no group leader and never forks: it makes the session,
            // execs the wrapper, and the keeper stays attached until the wrapper
            // exits, forwarding its status. Two statements, so no shell turns the
            // call into an `exec` and puts the leader back.
            wrapper_shell.to_string(),
            "-c".to_string(),
            r#"setsid "$@"; s=$?; exit "$s""#.to_string(),
            wrapper_shell.to_string(),
            // The wrapper, in its own session: its pid is the pgid it records, it
            // runs the step, and it writes the step's exit status beside the pgid
            // — the status file stays the source of truth, not the client's code.
            wrapper_shell.to_string(),
            "-c".to_string(),
            // Both files are written to a temporary name and renamed into place.
            // Rename within a directory is atomic on POSIX, so the host can never
            // read a half-written pgid or status.
            r#"p="$1"; shift; echo $$ > "$p.tmp"; mv "$p.tmp" "$p"; "$@"; s=$?; echo "$s" > "$p.status.tmp"; mv "$p.status.tmp" "$p.status"; exit "$s""#
                .to_string(),
            wrapper_shell.to_string(),
            pgid_in_container,
        ]);
        argv.push(spec.program.to_string());
        argv.extend(spec.args.iter().map(ToString::to_string));

        let mut command = Command::new("docker");
        command
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| EnvError::Spawn {
            program: SmolStr::new("docker exec"),
            source:  e,
        })?;

        let (tx, rx) = mpsc::channel(256);
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, ir::LogStream::Stdout, tx.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, ir::LogStream::Stderr, tx.clone()));
        }
        drop(tx);

        Ok(Box::new(DockerProcess {
            container: self.container.clone(),
            child,
            status_file: pgid_host.with_extension("status"),
            pgid_file: pgid_host,
            pgid: None,
            lines: Some(rx),
        }))
    }

    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> &str {
        HOST_ALIAS
    }

    fn ambient_env(&self, name: &str) -> Option<String> {
        self.ambient.get(name).cloned()
    }

    // `shares_host_filesystem` stays the default `false`: nothing of the host
    // is mounted into the container but the workspace, at its own path.

    // The workspace is bind-mounted from the run directory, so the host filesystem
    // answers for the container. A remote executor would go through its transport.
    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        match fs::read(self.workspace.join(relative)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(EnvError::workspace("read", relative.display(), e)),
        }
    }

    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let path = self.workspace.join(relative);
        let file = match File::open(path).await {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(EnvError::workspace("open", relative.display(), e));
            }
        };
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| EnvError::workspace("read", relative.display(), e))?;
        if bytes.len() > limit {
            return Err(EnvError::workspace(
                "read",
                relative.display(),
                executor::oversized_read(limit),
            ));
        }
        Ok(Some(bytes))
    }

    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| EnvError::workspace("create", parent.display(), e))?;
        }
        fs::write(&path, contents)
            .await
            .map_err(|e| EnvError::workspace("write", relative.display(), e))
    }

    fn grace(&self) -> Duration {
        self.grace
    }
}

struct DockerProcess {
    container:   String,
    child:       Child,
    pgid_file:   PathBuf,
    status_file: PathBuf,
    pgid:        Option<i32>,
    lines:       Option<LineStream>,
}

impl DockerProcess {
    /// The exit status the wrapper recorded, if it got that far.
    async fn recorded_status(&self) -> Option<ExitStatus> {
        let text = fs::read_to_string(&self.status_file).await.ok()?;
        text.trim().parse::<i32>().ok().map(ExitStatus::code)
    }

    /// Whether the step's process group still has anything in it.
    async fn is_group_alive(&self) -> bool {
        let Some(pgid) = self.pgid else {
            return false;
        };
        signal_group(&self.container, pgid, "0").await.is_ok()
    }
}

impl DockerProcess {
    /// Read the pgid the step recorded, waiting briefly for it to appear. A
    /// cancel can arrive before the step has written it.
    async fn pgid(&mut self) -> Option<i32> {
        if let Some(pgid) = self.pgid {
            return Some(pgid);
        }
        let deadline = Instant::now() + PGID_WAIT;
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(&self.pgid_file).await
                && let Ok(pgid) = text.trim().parse::<i32>()
                && pgid > 0
            {
                self.pgid = Some(pgid);
                return Some(pgid);
            }
            time::sleep(LIVENESS_POLL).await;
        }
        None
    }
}

#[async_trait]
impl ProcessHandle for DockerProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        let client = self.child.wait().await.map_err(EnvError::Wait)?;

        // The client returning does not mean the step is over, in two distinct ways,
        // and the group is the authority on both.
        //
        // If `setsid` forked, the client returned as soon as the child was running.
        //
        // And after a `SIGTERM`, the wrapper — which does not trap anything — dies at
        // once, so the client returns while a step that *does* trap `TERM` is still
        // running inside. Believing the client there would report a graceful exit,
        // skip the escalation to `SIGKILL`, and leave the step running until the
        // container was torn down.
        //
        // So: wait until a status is recorded or the process group is gone. `SIGKILL`
        // guarantees the second, which is what bounds this loop.
        let mut status = self.recorded_status().await;
        let mut status_source = "recorded";
        if status.is_none() {
            status_source = "recorded_late";
            let _ = self.pgid().await;
            loop {
                if let Some(recorded) = self.recorded_status().await {
                    status = Some(recorded);
                    break;
                }
                if !self.is_group_alive().await {
                    // One last look: the wrapper may have written on its way out.
                    status = self.recorded_status().await;
                    break;
                }
                time::sleep(LIVENESS_POLL).await;
            }
            if status.is_none() {
                status_source = "client";
            }
        }

        let _ = fs::remove_file(&self.pgid_file).await;
        let _ = fs::remove_file(&self.status_file).await;
        let status = status.unwrap_or_else(|| ExitStatus::from(client));
        // Which of the three answered is the subtlety the status itself never
        // carries: the wrapper's file, that file read after the group went away,
        // or the `docker exec` client's own code, which this module's header
        // explains can lie.
        tracing::debug!(
            container = %self.container,
            exit_code = status.code,
            exit_signal = status.signal,
            status_source,
            "step process ended"
        );
        Ok(status)
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        let Some(pgid) = self.pgid().await else {
            // The step never got far enough to record a group. Killing the `docker
            // exec` client is all that is left, and it is enough: there is nothing
            // inside to reach.
            tracing::warn!(
                container = %self.container,
                signal = sig.name(),
                "step process group was never recorded"
            );
            let _ = self.child.start_kill();
            return Ok(());
        };
        // Signal the group INSIDE the container. `docker kill` would hit PID 1 and
        // miss the step entirely.
        let target = format!("-{pgid}");
        let signal = format!("-{}", sig.number());
        // An error means the group is already gone, which is what we wanted
        // anyway.
        let _ = run_docker(&["exec", &self.container, "kill", &signal, "--", &target]).await;
        Ok(())
    }
}

/// Signal a process group inside a container.
///
/// Written `kill -SIG -PGID`, with **no `--` separator**. The handoff spells it
/// `kill -<SIG> -- -<PGID>`, which is the POSIX form, but busybox rejects it —
/// `sh: invalid number '--'` — and a rejected signal is a silent one: the step
/// keeps running and the ladder waits out its whole grace period for nothing.
/// Since alpine is the obvious base image, the separator cannot be used. `kill
/// -TERM -123` is understood by busybox ash, dash and bash alike.
///
/// It also goes through `sh -c` rather than as a bare `docker exec … kill`, so
/// this is the shell builtin rather than whichever `kill` binary the image
/// happens to carry.
async fn signal_group(container: &str, pgid: i32, signal: &str) -> Result<(), EnvError> {
    let script = format!("kill -{signal} -{pgid}");
    run_docker(&["exec", container, "sh", "-c", &script])
        .await
        .map(|_| ())
}

/// `docker inspect`'s `.Config.Env` — a JSON array of `KEY=VALUE` strings —
/// as a map. Docker appends `-e` flags after the image's entries, so on a
/// duplicate the later, stronger value wins.
///
/// Anything else is an error, not an empty map: the ambient env feeds `if:`
/// conditions, and a silently empty env could flip one. Entries without `=`
/// stay tolerated — docker itself accepts them — and are dropped.
fn parse_env_list(json: &str) -> Result<BTreeMap<String, String>, EnvError> {
    // Go's `{{json .Config.Env}}` renders a nil env as `null`, and a container
    // genuinely can have no `Config.Env`, so `null` deliberately means empty.
    let entries: Option<Vec<String>> =
        serde_json::from_str(json).map_err(|error| EnvError::Backend {
            backend:   SmolStr::new("docker"),
            operation: SmolStr::new("inspect"),
            message:   format!(
                "`.Config.Env` output was not a JSON string array ({} bytes): {error}",
                json.len()
            ),
        })?;
    Ok(entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
        })
        .collect())
}

pub(crate) fn next_token() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[tracing::instrument(
    name = "docker.cli",
    level = "debug",
    skip_all,
    fields(operation = args.first().copied().unwrap_or("docker"))
)]
pub(crate) async fn run_docker(args: &[&str]) -> Result<String, EnvError> {
    let operation = args.first().copied().unwrap_or("docker");
    let backend_error = |message: String| EnvError::Backend {
        backend: SmolStr::new("docker"),
        operation: SmolStr::new(operation),
        message,
    };
    let wait = docker_wait(operation);
    let output = time::timeout(
        wait,
        Command::new("docker")
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| backend_error(format!("timed out after {}s", wait.as_secs())))?
    .map_err(|e| backend_error(e.to_string()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(backend_error(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// A wedged daemon fails the operation instead of hanging the run — release
/// included, which the run report awaits. Image transfer and builds are
/// legitimately slow on cold caches; everything else is control-plane work.
const DOCKER_CLI_WAIT: Duration = Duration::from_secs(300);
const DOCKER_IMAGE_WAIT: Duration = Duration::from_secs(1800);

/// The bound for one `docker` CLI invocation, by its leading operation.
fn docker_wait(operation: &str) -> Duration {
    match operation {
        "pull" | "build" => DOCKER_IMAGE_WAIT,
        _ => DOCKER_CLI_WAIT,
    }
}

/// [`run_docker`], with `input` written to the child's stdin — how a login's
/// password travels without ever being an argument.
#[tracing::instrument(
    name = "docker.cli",
    level = "debug",
    skip_all,
    fields(operation = operation)
)]
async fn run_docker_stdin(
    operation: &str,
    args: &[&str],
    input: &[u8],
) -> Result<String, EnvError> {
    let operation = SmolStr::new(operation);
    let backend_error = |message: String| EnvError::Backend {
        backend: SmolStr::new("docker"),
        operation: operation.clone(),
        message,
    };
    let exchange = async {
        let mut child = Command::new("docker")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| backend_error(e.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input)
                .await
                .map_err(|e| backend_error(e.to_string()))?;
        }
        child
            .wait_with_output()
            .await
            .map_err(|e| backend_error(e.to_string()))
    };
    let output = time::timeout(DOCKER_CLI_WAIT, exchange)
        .await
        .map_err(|_| backend_error(format!("timed out after {}s", DOCKER_CLI_WAIT.as_secs())))??;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(backend_error(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// The run dir's recorded run id, or a fresh one recorded now — on disk before
/// any container carries it, so a crash can never leave a container whose name
/// no later process can rebuild. One executor per run dir is the rule
/// (workspaces would collide otherwise), so write-then-rename is enough: a
/// reader sees the whole id or none.
async fn load_or_record_run_id(run_dir: &Path) -> Result<SmolStr, EnvError> {
    let path = run_dir.join(RUN_ID_FILE);
    let io_error =
        |action, path: &Path, e: io::Error| EnvError::workspace(action, path.display(), e);
    match fs::read_to_string(&path).await {
        Ok(recorded) if !recorded.trim().is_empty() => return Ok(SmolStr::new(recorded.trim())),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io_error("read", &path, e)),
    }

    let minted = fresh_run_id();
    fs::create_dir_all(run_dir)
        .await
        .map_err(|e| io_error("create", run_dir, e))?;
    let staged = run_dir.join(format!("{RUN_ID_FILE}.tmp"));
    let mut file = File::create(&staged)
        .await
        .map_err(|e| io_error("create", &staged, e))?;
    file.write_all(minted.as_bytes())
        .await
        .map_err(|e| io_error("write", &staged, e))?;
    file.sync_all()
        .await
        .map_err(|e| io_error("sync", &staged, e))?;
    fs::rename(&staged, &path)
        .await
        .map_err(|e| io_error("rename", &path, e))?;
    Ok(SmolStr::new(minted))
}

/// Unique across processes and time: two runs on one daemon must never share a
/// container name, or one run's fence would remove the other's containers.
fn fresh_run_id() -> String {
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

/// Containers this run left behind, for leak checks in tests.
pub async fn list_containers(prefix: &str) -> Vec<String> {
    run_docker(&["ps", "-a", "--format", "{{.Names}}"])
        .await
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|n| n.starts_with(prefix))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{parse_env_list, registry_host};

    #[test]
    fn env_lists_parse_with_the_later_duplicate_winning() {
        let parsed = parse_env_list(r#"["PATH=/usr/bin","A=first","A=second","EMPTY=","BARE"]"#)
            .expect("a string array parses");
        assert_eq!(parsed.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(parsed.get("A").map(String::as_str), Some("second"));
        assert_eq!(parsed.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(parsed.get("BARE"), None);
    }

    #[test]
    fn a_nil_env_is_empty_and_anything_else_unparseable_is_an_error() {
        let parsed = parse_env_list("null").expect("a nil `Config.Env` renders as `null`");
        assert!(parsed.is_empty());
        assert!(parse_env_list("not json").is_err());
        assert!(parse_env_list("{}").is_err());
    }

    #[test]
    fn registry_hosts_parse_from_image_references() {
        assert_eq!(registry_host("alpine:3.20"), None);
        assert_eq!(registry_host("library/alpine"), None);
        assert_eq!(registry_host("ghcr.io/acme/tool:1"), Some("ghcr.io"));
        assert_eq!(
            registry_host("localhost:5000/acme/tool"),
            Some("localhost:5000")
        );
        assert_eq!(registry_host("localhost/acme/tool"), Some("localhost"));
    }
}

#[cfg(test)]
mod platform_fallback_tests {
    use super::*;

    #[test]
    fn only_an_architecture_miss_triggers_the_fallback() {
        let miss = EnvError::Backend {
            backend:   SmolStr::new("docker"),
            operation: SmolStr::new("pull"),
            message:   "no matching manifest for linux/arm64/v8 in the manifest list entries"
                .to_string(),
        };
        assert!(is_missing_platform(&miss));
        let denied = EnvError::Backend {
            backend:   SmolStr::new("docker"),
            operation: SmolStr::new("pull"),
            message:   "pull access denied for ghcr.io/x/y".to_string(),
        };
        assert!(!is_missing_platform(&denied));
    }
}
