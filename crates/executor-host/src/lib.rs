//! The host executor: a workspace directory and real processes on this machine.
//!
//! Implements the [`executor`] interface with nothing in between a step and the
//! operating system. Each step is spawned into its own process group, so a `run:`
//! script that backgrounds children can be signalled — and dies — as a unit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    EnvError, EnvHandle, ExecEnv, Executor, ExitStatus, LineStream, ProcessHandle, ProcessSpec,
    ReleaseReport, Retention, ScopeOutcome, ScopeSpec, Sig,
};
use smol_str::SmolStr;
use tokio::sync::mpsc;

/// Runs steps as processes on this machine.
pub struct HostExecutor {
    run_dir: PathBuf,
    retention: Retention,
}

impl HostExecutor {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
            retention: Retention::default(),
        }
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn workspace_for(&self, instance: &str) -> PathBuf {
        self.run_dir.join("scopes").join(instance).join("work")
    }
}

/// What release needs: the workspace, and whether to keep it.
#[derive(Clone, Debug)]
struct HostTeardown {
    path: PathBuf,
    retention: Retention,
}

#[async_trait]
impl Executor for HostExecutor {
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError> {
        let workspace = self.workspace_for(&scope.instance);
        tokio::fs::create_dir_all(&workspace)
            .await
            .map_err(|e| EnvError::Workspace {
                path: workspace.display().to_string(),
                message: e.to_string(),
            })?;
        Ok(EnvHandle::new(
            scope.id,
            scope.instance.clone(),
            Arc::new(HostEnv {
                workspace: workspace.clone(),
                workspace_str: workspace.display().to_string(),
                env: scope.env.clone(),
                grace: scope.grace,
            }),
            HostTeardown {
                path: workspace,
                retention: self.retention,
            },
        ))
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let Some(HostTeardown { path, retention }) = env.teardown::<HostTeardown>() else {
            return report.problem("host executor was handed a foreign environment");
        };
        if retention.keeps(outcome) {
            report.workspace_kept = Some(path.display().to_string());
            return report;
        }
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => report.workspace_removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => report.workspace_removed = true,
            Err(e) => report = report.problem(format!("could not remove {}: {e}", path.display())),
        }
        report
    }
}

struct HostEnv {
    workspace: PathBuf,
    workspace_str: String,
    env: BTreeMap<SmolStr, SmolStr>,
    grace: Duration,
}

#[async_trait]
impl ExecEnv for HostEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let cwd = match &spec.cwd {
            Some(rel) => self.workspace.join(rel),
            None => self.workspace.clone(),
        };
        tokio::fs::create_dir_all(&cwd)
            .await
            .map_err(|e| EnvError::Workspace {
                path: cwd.display().to_string(),
                message: e.to_string(),
            })?;

        let mut command = tokio::process::Command::new(spec.program.as_str());
        command
            .args(spec.args.iter().map(|a| a.as_str()))
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in self.env.iter().chain(spec.env.iter()) {
            command.env(key.as_str(), value.as_str());
        }
        // Its own process group, so the whole tree can be signalled as a unit. A
        // `run:` script that backgrounds children must die with them.
        command.process_group(0);

        let mut child = command.spawn().map_err(|e| EnvError::Spawn {
            program: spec.program.clone(),
            message: e.to_string(),
        })?;
        let pid = child.id().ok_or_else(|| EnvError::Spawn {
            program: spec.program.clone(),
            message: "the child exited before its pid could be read".into(),
        })? as i32;

        let (tx, rx) = mpsc::channel(256);
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, ir::LogStream::Stdout, tx.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, ir::LogStream::Stderr, tx.clone()));
        }
        drop(tx);

        Ok(Box::new(HostProcess {
            child,
            pgid: pid,
            lines: Some(rx),
        }))
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn workspace_in_env(&self) -> &str {
        &self.workspace_str
    }

    fn grace(&self) -> Duration {
        self.grace
    }
}

struct HostProcess {
    child: tokio::process::Child,
    /// Equal to the child's pid, because the child leads its own group.
    pgid: i32,
    lines: Option<LineStream>,
}

#[async_trait]
impl ProcessHandle for HostProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| EnvError::Wait(e.to_string()))?;
        Ok(ExitStatus::from(status))
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        // killpg, never kill: the group is the unit.
        //
        // SAFETY: `killpg` takes a pgid and a signal number and has no memory
        // effects. ESRCH means the group is already gone, which is success for our
        // purposes — the ladder is idempotent.
        let result = unsafe { libc::killpg(self.pgid, sig.number()) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            _ => Err(EnvError::Signal(format!(
                "killpg({}, {}) failed: {error}",
                self.pgid,
                sig.name()
            ))),
        }
    }
}
