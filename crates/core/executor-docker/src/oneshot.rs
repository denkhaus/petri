//! One-shot containers: the [`ContainerRunner`] the Docker executors bind to a
//! scope at acquisition.
//!
//! One `docker run` per invocation, named under the scope's one-shot prefix so
//! the acquire fence and scope release can sweep crash leftovers by name. The
//! entrypoint is PID 1, so signalling is plain `docker kill` — none of the
//! `setsid` machinery long-lived scope containers need — and the `docker run`
//! client's exit code is the container's own.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    ContainerImage, ContainerRunner, EnvError, ExitStatus, LineStream, OneShotContainer,
    ProcessHandle, Progress, ProgressSink, Sig,
};
use ir::ScopeId;
use smol_str::SmolStr;
use tokio::sync::mpsc;

use crate::{CONTAINER_WORKSPACE, PullPolicy, prepare_registry_image, run_docker};

/// Runs one-shot containers in one scope's world.
pub(crate) struct OneShotRunner {
    /// Container-name prefix for this scope's one-shots (`petri-<run>-<inst>-s`).
    pub(crate) prefix: String,
    /// The scope's workspace on the host, mounted at [`CONTAINER_WORKSPACE`].
    pub(crate) workspace: PathBuf,
    /// `--network` value — the job container's namespace for a containerized
    /// scope, the scope's network for a host scope with services. `None` is the
    /// daemon default.
    pub(crate) network: Option<String>,
    pub(crate) pull: PullPolicy,
    pub(crate) scope: ScopeId,
    pub(crate) progress: Arc<dyn ProgressSink>,
}

impl OneShotRunner {
    /// The image reference to run: a registry image pulled under the policy, or
    /// a workspace Dockerfile built once per tag (the tag is the cache key, so
    /// a rebuilt run reuses it).
    async fn prepare(&self, image: &ContainerImage) -> Result<SmolStr, EnvError> {
        match image {
            ContainerImage::Registry { image } => {
                prepare_registry_image(image, self.pull, self.scope, &self.progress).await?;
                Ok(image.clone())
            }
            ContainerImage::Build { context, tag } => {
                if run_docker(&["image", "inspect", tag]).await.is_ok() {
                    return Ok(tag.clone());
                }
                let context = self.workspace.join(context);
                self.progress
                    .progress(self.scope, Progress::BuildingImage { tag: tag.clone() });
                let context = context.display().to_string();
                run_docker(&["build", "-t", tag, &context]).await?;
                Ok(tag.clone())
            }
        }
    }
}

#[async_trait]
impl ContainerRunner for OneShotRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let image = self.prepare(&spec.image).await?;
        let name = format!("{}{}-{}", self.prefix, std::process::id(), next_token());

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
        if let Some(network) = &self.network {
            argv.push("--network".into());
            argv.push(network.clone());
        }
        if let Some(entrypoint) = &spec.entrypoint {
            argv.push("--entrypoint".into());
            argv.push(entrypoint.to_string());
        }
        for (key, value) in &spec.env {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
        }
        argv.push(image.to_string());
        argv.extend(spec.args.iter().map(|a| a.to_string()));

        let mut command = tokio::process::Command::new("docker");
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
    name: String,
    child: tokio::process::Child,
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

fn next_token() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
