//! One-shot containers: the [`ContainerRunner`] the Docker backend binds to a
//! container scope, for GitHub `uses: docker://…` and local-Dockerfile action
//! steps.
//!
//! One `docker run` per invocation, named under the scope's one-shot prefix so
//! the acquire fence and scope release sweep crash leftovers by name. The
//! scope's **one-shot marker** gates both sweeps: it is written under the
//! scope dir before the first container exists, so a scope that never ran a
//! Docker action never spawns a `docker` client for a sweep with nothing to
//! look for. The entrypoint is PID 1, so signalling is plain `docker kill`,
//! and the client's exit code is the container's own. The container shares
//! the job container's network namespace, so it reaches the job's services
//! and the host alias the same way the job does. It runs against the local
//! Docker daemon through the `docker` CLI, independent of the sandbox-driver
//! provider that owns the job container.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use std::{fmt, io};

use async_trait::async_trait;
use executor::lines::{LINE_CHANNEL_CAPACITY, pump};
use executor::{
    AcquireContext, ContainerImage, ContainerRunner, EnvError, ExitStatus, LineStream,
    OneShotContainer, ProcessHandle, Progress, ProgressSink, ScopeSpec, Sig,
};
use ir::ScopeId;
use smol_str::SmolStr;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{OnceCell, mpsc};
use tokio::{fs, time};

use crate::{CONTAINER_WORKSPACE, DOCKER_HOST_ALIAS};

/// The `--add-host` argument that backs the host alias on every daemon.
const ADD_HOST_GATEWAY: &str = "--add-host=host.docker.internal:host-gateway";
const DOCKER_CLI_WAIT: Duration = Duration::from_secs(300);
const DOCKER_IMAGE_WAIT: Duration = Duration::from_secs(1800);
/// The one fallback platform: CI images target linux/amd64, so an image with
/// no manifest for this daemon's architecture is retried as amd64.
const FALLBACK_PLATFORM: &str = "linux/amd64";

fn next_token() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A one-shot container name, distinct from its prefix so a kill target and a
/// sweep pattern can never swap.
#[derive(Clone, Debug)]
struct ContainerName(String);

impl ContainerName {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A container-name prefix — a scope's one-shot prefix. What sweep matching
/// starts from; never itself a name.
#[derive(Clone, Debug)]
pub(crate) struct ContainerPrefix(String);

impl ContainerPrefix {
    pub(crate) fn new(prefix: String) -> Self {
        Self(prefix)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn join(&self, suffix: &str) -> ContainerName {
        ContainerName(format!("{}{suffix}", self.0))
    }
}

/// The per-spawn env files under a scope dir. Never retained: they can hold
/// resolved secrets.
fn env_files_dir(scope_dir: &Path) -> PathBuf {
    scope_dir.join("exec-env")
}

/// The one-shot marker under a scope dir; see the module docs.
fn marker_path(scope_dir: &Path) -> PathBuf {
    scope_dir.join("one-shots")
}

/// Runs one-shot containers in one scope's world.
pub(crate) struct OneShotRunner {
    prefix:    ContainerPrefix,
    workspace: PathBuf,
    env_files: PathBuf,
    env:       BTreeMap<SmolStr, SmolStr>,
    /// `--network` value: the job container's namespace (`container:<job>`).
    network:   Option<String>,
    scope:     ScopeId,
    progress:  Arc<dyn ProgressSink>,
    marker:    PathBuf,
    marked:    OnceCell<()>,
    ensured:   Mutex<HashSet<SmolStr>>,
}

impl OneShotRunner {
    /// A runner for `scope`, whose containers mount `workspace` and whose
    /// env files and marker live under `scope_dir`.
    pub(crate) fn new(
        prefix: ContainerPrefix,
        workspace: PathBuf,
        scope_dir: &Path,
        scope: &ScopeSpec,
        network: Option<String>,
        ctx: &AcquireContext,
    ) -> Self {
        Self {
            prefix,
            workspace,
            env_files: env_files_dir(scope_dir),
            env: scope.env.clone(),
            network,
            scope: scope.id,
            progress: ctx.progress().clone(),
            marker: marker_path(scope_dir),
            marked: OnceCell::new(),
            ensured: Mutex::new(HashSet::new()),
        }
    }

    /// The image reference to run: a registry image pulled when absent, or a
    /// workspace Dockerfile built under its tag. Ensured once per reference,
    /// except a non-`reuse` build, which rebuilds every time.
    async fn prepare(&self, image: &ContainerImage) -> Result<SmolStr, EnvError> {
        let (reference, memoize) = match image {
            ContainerImage::Registry { image } => (image.clone(), true),
            ContainerImage::Build { tag, reuse, .. } => (tag.clone(), *reuse),
        };
        if memoize
            && self
                .ensured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&reference)
        {
            return Ok(reference);
        }
        match image {
            ContainerImage::Registry { image } => {
                prepare_registry_image(image, self.scope, self.progress.as_ref()).await?;
            }
            ContainerImage::Build {
                context,
                dockerfile,
                tag,
                reuse,
            } => {
                if !(*reuse && run_docker(&["image", "inspect", tag]).await.is_ok()) {
                    let context = self.workspace.join(context);
                    self.progress
                        .progress(self.scope, Progress::BuildingImage { tag: tag.clone() });
                    let context = context.display().to_string();
                    match dockerfile {
                        Some(file) => {
                            let file = format!("{context}/{file}");
                            run_docker(&["build", "-t", tag, "-f", &file, &context]).await?;
                        }
                        None => {
                            run_docker(&["build", "-t", tag, &context]).await?;
                        }
                    }
                }
            }
        }
        if memoize {
            self.ensured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(reference.clone());
        }
        Ok(reference)
    }

    /// Record the scope's one-shot marker, once, before the first container
    /// exists, so a crash right after the launch still leaves the mark.
    async fn mark(&self) -> Result<(), EnvError> {
        self.marked
            .get_or_try_init(|| async {
                if let Some(parent) = self.marker.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|e| EnvError::workspace("create", parent.display(), e))?;
                }
                fs::write(&self.marker, b"")
                    .await
                    .map_err(|e| EnvError::workspace("write", self.marker.display(), e))
            })
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ContainerRunner for OneShotRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> &str {
        DOCKER_HOST_ALIAS
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        self.mark().await?;
        let image = self.prepare(&spec.image).await?;
        let token = format!("{}-{}", process::id(), next_token());
        let name = self.prefix.join(&token);

        let mount = format!("{}:{CONTAINER_WORKSPACE}", self.workspace.display());
        let workdir = spec
            .workdir
            .as_deref()
            .unwrap_or(CONTAINER_WORKSPACE)
            .to_string();
        let mut argv: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--init".into(),
            "--name".into(),
            name.to_string(),
            "-v".into(),
            mount,
            "-w".into(),
            workdir,
        ];
        match &self.network {
            Some(network) => {
                argv.push("--network".into());
                argv.push(network.clone());
                // `container:<job>` shares the job's netns — `--add-host` is
                // refused there and unneeded; every other network gets it.
                if !network.starts_with("container:") {
                    argv.push(ADD_HOST_GATEWAY.into());
                }
            }
            None => argv.push(ADD_HOST_GATEWAY.into()),
        }
        if let Some(entrypoint) = &spec.entrypoint {
            argv.push("--entrypoint".into());
            argv.push(entrypoint.to_string());
        }
        // The scope's env first, the spec's own on top, as a 0600 env file
        // plus the client's own process env: the spec env can hold resolved
        // secrets, and `-e KEY=VALUE` argv shows them to every local user.
        let merged = self
            .env
            .iter()
            .filter(|(k, _)| !spec.env.contains_key(*k))
            .chain(spec.env.iter());
        let split = split_env(merged);
        fs::create_dir_all(&self.env_files)
            .await
            .map_err(|e| EnvError::workspace("create", self.env_files.display(), e))?;
        let env_file = self.env_files.join(format!("{token}.env"));
        write_env_file(&env_file, &split.file)
            .await
            .map_err(|e| EnvError::workspace("write", env_file.display(), e))?;
        argv.push("--env-file".into());
        argv.push(env_file.display().to_string());
        for (key, _) in &split.inherit {
            argv.push("-e".into());
            argv.push(key.to_string());
        }
        argv.push(image.to_string());
        argv.extend(spec.args.iter().map(ToString::to_string));

        let mut command = Command::new("docker");
        command
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &split.inherit {
            command.env(key.as_str(), value.as_str());
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = fs::remove_file(&env_file).await;
                return Err(EnvError::Spawn {
                    program: SmolStr::new("docker run"),
                    source:  e,
                });
            }
        };

        let (tx, rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, ir::LogStream::Stdout, tx.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, ir::LogStream::Stderr, tx.clone()));
        }
        drop(tx);

        Ok(Box::new(OneShotProcess {
            name,
            child,
            env_file,
            lines: Some(rx),
        }))
    }
}

struct OneShotProcess {
    name:     ContainerName,
    child:    Child,
    env_file: PathBuf,
    lines:    Option<LineStream>,
}

#[async_trait]
impl ProcessHandle for OneShotProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        let status = self.child.wait().await;
        let _ = fs::remove_file(&self.env_file).await;
        let status = status.map_err(EnvError::Wait)?;
        Ok(ExitStatus::from(status))
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        let result = match sig {
            Sig::Term => run_docker(&["kill", "-s", "TERM", self.name.as_str()]).await,
            Sig::Kill => run_docker(&["kill", self.name.as_str()]).await,
        };
        let _ = result;
        Ok(())
    }
}

/// Sweeps a scope's one-shot world: every container under `prefix`, and the
/// env files under `scope_dir`. The marker gates it — no marker, no one-shot
/// ever launched, no `docker` client spawned. Best-effort: a down daemon has
/// nothing of ours to remove.
pub(crate) async fn sweep_scope(prefix: &ContainerPrefix, scope_dir: &Path) {
    if !fs::try_exists(marker_path(scope_dir))
        .await
        .unwrap_or(false)
    {
        return;
    }
    let names = list_containers(prefix.as_str());
    let (names, _) = tokio::join!(names, fs::remove_dir_all(env_files_dir(scope_dir)));
    for name in names {
        let _ = run_docker(&["rm", "-f", "-v", &name]).await;
    }
}

/// Every container on the local daemon whose name starts with `prefix`. For
/// tests and sweeps that check a run left nothing behind; best-effort, empty
/// when no daemon answers.
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

async fn image_present(image: &str) -> bool {
    run_docker(&["image", "inspect", image]).await.is_ok()
}

async fn prepare_registry_image(
    image: &str,
    scope: ScopeId,
    progress: &dyn ProgressSink,
) -> Result<(), EnvError> {
    if !image_present(image).await {
        progress.progress(scope, Progress::PullingImage {
            image: SmolStr::new(image),
        });
        pull_image(image).await?;
    }
    Ok(())
}

fn is_missing_platform(error: &EnvError) -> bool {
    matches!(error, EnvError::Backend { message, .. } if message.contains("no matching manifest"))
}

async fn pull_image(image: &str) -> Result<(), EnvError> {
    let Err(error) = run_docker(&["pull", image]).await else {
        return Ok(());
    };
    if !is_missing_platform(&error) {
        return Err(error);
    }
    match run_docker(&["pull", "--platform", FALLBACK_PLATFORM, image]).await {
        Ok(_) => {
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

struct SplitEnv {
    file:    String,
    inherit: Vec<(SmolStr, SmolStr)>,
}

/// One spawn's env, split for the docker CLI so no value becomes an argument.
fn split_env<'a>(env: impl IntoIterator<Item = (&'a SmolStr, &'a SmolStr)>) -> SplitEnv {
    let mut split = SplitEnv {
        file:    String::new(),
        inherit: Vec::new(),
    };
    for (key, value) in env {
        if env_file_representable(key, value) {
            split.file.push_str(key);
            split.file.push('=');
            split.file.push_str(value);
            split.file.push('\n');
        } else {
            split.inherit.push((key.clone(), value.clone()));
        }
    }
    split
}

fn env_file_representable(key: &str, value: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('#')
        && !key.starts_with(char::is_whitespace)
        && !key.contains(['\n', '\r'])
        && !value.contains(['\n', '\r'])
}

async fn write_env_file(path: &Path, contents: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(contents.as_bytes()).await
}

fn docker_wait(operation: &str) -> Duration {
    match operation {
        "pull" | "build" => DOCKER_IMAGE_WAIT,
        _ => DOCKER_CLI_WAIT,
    }
}

async fn run_docker(args: &[&str]) -> Result<String, EnvError> {
    let operation = args.first().copied().unwrap_or("docker");
    let backend_error = |message: String| EnvError::backend("docker", operation, message);
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
