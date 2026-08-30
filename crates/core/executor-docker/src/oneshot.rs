//! One-shot containers: the [`ContainerRunner`] the Docker executors bind to a
//! scope at acquisition.
//!
//! One `docker run` per invocation, named under the scope's one-shot prefix so
//! the acquire fence and scope release can sweep crash leftovers by name. The
//! entrypoint is PID 1, so signalling is plain `docker kill` — none of the
//! `setsid` machinery long-lived scope containers need — and the `docker run`
//! client's exit code is the container's own.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::process::{self, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    AcquireContext, ContainerImage, ContainerRunner, EnvError, ExitStatus, LineStream,
    OneShotContainer, ProcessHandle, Progress, ProgressSink, ScopeSpec, Sig,
};
use ir::ScopeId;
use smol_str::SmolStr;
use tokio::fs;
use tokio::process::{Child, Command};
use tokio::sync::{OnceCell, mpsc};

use crate::{CONTAINER_WORKSPACE, PullPolicy, next_token, prepare_registry_image, run_docker};

/// Runs one-shot containers in one scope's world.
pub(crate) struct OneShotRunner {
    /// Container-name prefix for this scope's one-shots
    /// (`petri-<run>-<inst>-s`).
    prefix:    String,
    /// The scope's workspace on the host, mounted at [`CONTAINER_WORKSPACE`].
    workspace: PathBuf,
    /// The scope's resolved env: one-shots live in the scope's world, so they
    /// see what every process of the scope sees. A spec's own env wins.
    env:       BTreeMap<SmolStr, SmolStr>,
    /// `--network` value — the job container's namespace for a containerized
    /// scope, the scope's network for a host scope with services. `None` is the
    /// daemon default.
    network:   Option<String>,
    pull:      PullPolicy,
    scope:     ScopeId,
    progress:  Arc<dyn ProgressSink>,
    /// The scope's one-shot marker, recorded before the first launch so the
    /// acquire fence and release sweep of a Docker-free scope know whether
    /// there can be anything to look for.
    marker:    PathBuf,
    marked:    OnceCell<()>,
    /// Image references this runner has already ensured. Presence cannot
    /// regress within a run, so a `pre`/`main`/`post` trio — or N steps naming
    /// one image — costs one probe, not N.
    ensured:   Mutex<HashSet<SmolStr>>,
}

impl OneShotRunner {
    pub(crate) fn new(
        prefix: String,
        workspace: PathBuf,
        marker: PathBuf,
        scope: &ScopeSpec,
        network: Option<String>,
        pull: PullPolicy,
        ctx: &AcquireContext,
    ) -> Self {
        Self {
            prefix,
            workspace,
            env: scope.env.clone(),
            network,
            pull,
            scope: scope.id,
            progress: Arc::clone(ctx.progress()),
            marker,
            marked: OnceCell::new(),
            ensured: Mutex::new(HashSet::new()),
        }
    }

    /// The image reference to run: a registry image pulled under the policy, or
    /// a workspace Dockerfile built under its tag (content-addressed tags are
    /// reused, across runs). Ensured once per reference — except a non-`reuse`
    /// build, which asks for a rebuild every time.
    async fn prepare(&self, image: &ContainerImage) -> Result<SmolStr, EnvError> {
        let (reference, memoize) = match image {
            ContainerImage::Registry { image } => (image.clone(), true),
            ContainerImage::Build { tag, reuse, .. } => (tag.clone(), *reuse),
        };
        if memoize
            && self
                .ensured
                .lock()
                .expect("the ensured-image set is not poisoned")
                .contains(&reference)
        {
            return Ok(reference);
        }
        match image {
            ContainerImage::Registry { image } => {
                prepare_registry_image(image, self.pull, self.scope, &self.progress).await?;
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
                .expect("the ensured-image set is not poisoned")
                .insert(reference.clone());
        }
        Ok(reference)
    }

    /// Record the scope's one-shot marker, once, before the first container
    /// exists — so a crash right after the launch still leaves the mark a
    /// resuming fence looks for.
    async fn mark(&self) -> Result<(), EnvError> {
        self.marked
            .get_or_try_init(|| async {
                let io_error = |e: io::Error| EnvError::Workspace {
                    path:    self.marker.display().to_string(),
                    message: e.to_string(),
                };
                if let Some(parent) = self.marker.parent() {
                    fs::create_dir_all(parent).await.map_err(io_error)?;
                }
                fs::write(&self.marker, b"").await.map_err(io_error)
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
        crate::HOST_ALIAS
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        self.mark().await?;
        let image = self.prepare(&spec.image).await?;
        let name = format!("{}{}-{}", self.prefix, process::id(), next_token());

        let mount = format!("{}:{CONTAINER_WORKSPACE}", self.workspace.display());
        let workdir = spec
            .workdir
            .as_deref()
            .unwrap_or(CONTAINER_WORKSPACE)
            .to_string();
        // `--init` puts a signal-forwarding init at PID 1: without it a shell
        // entrypoint would *ignore* the TERM that `docker kill -s TERM` sends —
        // PID 1 gets no default signal dispositions — and every polite cancel
        // would wait out its whole grace period for nothing.
        let mut argv: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--init".into(),
            "--name".into(),
            name.clone(),
            "-v".into(),
            mount,
            "-w".into(),
            workdir,
        ];
        match &self.network {
            Some(network) => {
                argv.push("--network".into());
                argv.push(network.clone());
                // `container:<job>` shares the job container's network
                // namespace — `--add-host` is refused there, and unneeded: the
                // job container's own mapping answers. Every other network
                // gets the mapping directly.
                if !network.starts_with("container:") {
                    argv.push(crate::ADD_HOST_GATEWAY.into());
                }
            }
            None => argv.push(crate::ADD_HOST_GATEWAY.into()),
        }
        if let Some(entrypoint) = &spec.entrypoint {
            argv.push("--entrypoint".into());
            argv.push(entrypoint.to_string());
        }
        // The scope's env first, the spec's own on top.
        for (key, value) in self.env.iter().filter(|(k, _)| !spec.env.contains_key(*k)) {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
        }
        for (key, value) in &spec.env {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
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

        let mut child = command.spawn().map_err(|e| EnvError::Spawn {
            program: SmolStr::new("docker run"),
            message: e.to_string(),
        })?;

        let (tx, rx) = mpsc::channel(256);
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
            lines: Some(rx),
        }))
    }
}

struct OneShotProcess {
    name:  String,
    child: Child,
    lines: Option<LineStream>,
}

#[async_trait]
impl ProcessHandle for OneShotProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        // `docker run` forwards the container's exit code; the entrypoint is
        // PID 1, so there is no forked wrapper whose early return could lie.
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| EnvError::Wait(e.to_string()))?;
        Ok(ExitStatus::from(status))
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        // The entrypoint is PID 1: `docker kill` reaches it directly. A
        // container already gone is what we wanted anyway.
        let result = match sig {
            Sig::Term => run_docker(&["kill", "-s", "TERM", &self.name]).await,
            Sig::Kill => run_docker(&["kill", &self.name]).await,
        };
        let _ = result;
        Ok(())
    }
}
