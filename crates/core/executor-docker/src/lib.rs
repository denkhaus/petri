//! The Docker executor: a long-lived container with the workspace bind-mounted, and
//! steps run through `docker exec`.
//!
//! Implements the [`executor`] interface. The bind mount is what keeps the two sides
//! symmetric: [`ExecEnv::workspace`] is the host path, so artifacts and logs are
//! handled the same way whether or not a container is involved.
//!
//! # Why `docker exec … kill`, and not `docker kill`
//!
//! `docker kill` signals PID 1 of the container. Steps are not PID 1 — they are
//! `docker exec` processes, siblings of init — so `docker kill` never reaches them.
//! Cancelling a step is therefore `docker exec <c> kill -<SIG> -- -<PGID>`, run
//! inside the container against the step's own process group. Container-level kill is
//! reserved for scope release, where killing everything is the point.
//!
//! To have a process group to signal, each step is launched under `setsid` and
//! records its own pid into a file on the bind-mounted workspace, where the host can
//! read it. `setsid` makes the shell a session leader, so its pid *is* its pgid.
//! **The image must provide `setsid`** (busybox and util-linux both do).
//!
//! # Why the exit status comes from a file
//!
//! `setsid` forks when its caller is already a process group leader, and whether
//! `docker exec` hands its process one is not something to rely on. If it does fork,
//! `setsid` exits as soon as the child is running and `docker exec` returns 0
//! immediately — the step is still going, and its real exit status is lost.
//!
//! So the wrapper records the status itself, and [`DockerProcess::wait`] prefers that
//! file over the client's exit code, polling the process group's liveness rather than
//! trusting an early return. This makes the executor correct whichever way `setsid`
//! behaves, instead of correct on the platforms that happen to suit it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    EnvError, EnvHandle, ExecEnv, Executor, ExitStatus, LineStream, ProcessHandle, ProcessSpec,
    ReleaseReport, Retention, ScopeOutcome, ScopeSpec, Sig,
};
use ir::RuntimeTarget;
use smol_str::SmolStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OnceCell, mpsc};

/// Where the workspace is mounted inside the container.
pub const CONTAINER_WORKSPACE: &str = "/workspace";

/// The run id's file under the run dir. Container names carry the id and the
/// acquire fence is remove-by-name, so the id must outlive the process that
/// minted it: it is recorded here before the run's first container exists, and
/// any later executor over the same run dir — a resuming process's — reads it
/// back and reaches the same containers. A fork into a fresh run dir gets a
/// fresh id.
pub const RUN_ID_FILE: &str = "docker-run-id";

/// How often [`DockerProcess::wait`] checks whether the step's process group is
/// still alive, once the `docker exec` client has returned without a recorded status.
///
/// This sets two things at once: how long after a step really ends before the driver
/// notices, and how much idle `docker exec` traffic a long step costs. At 50ms a
/// ten-minute step costs about 12,000 liveness checks in the worst case — the case
/// where `setsid` forked. When it did not fork, which is the common case, the status
/// file is already there on the first look and the loop never runs.
pub const LIVENESS_POLL: Duration = Duration::from_millis(50);

/// How long to wait for the step to record its pgid before giving up on signalling
/// the group. A cancel can arrive before the step has written it.
pub const PGID_WAIT: Duration = Duration::from_secs(2);

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
    run_dir: PathBuf,
    /// Resolved on first use from [`RUN_ID_FILE`]: the run dir's recorded id, or
    /// a fresh one recorded there.
    run_id: OnceCell<SmolStr>,
    retention: Retention,
    pull: PullPolicy,
}

impl DockerExecutor {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
            run_id: OnceCell::new(),
            retention: Retention::default(),
            pull: PullPolicy::default(),
        }
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    pub fn with_pull_policy(mut self, pull: PullPolicy) -> Self {
        self.pull = pull;
        self
    }

    /// Whether a Docker daemon is reachable. Tests skip rather than fail without one.
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

    async fn ensure_image(&self, image: &str) -> Result<(), EnvError> {
        let present = run_docker(&["image", "inspect", image]).await.is_ok();
        match self.pull {
            PullPolicy::Never => Ok(()),
            PullPolicy::IfNotPresent if present => Ok(()),
            _ => run_docker(&["pull", image]).await.map(|_| ()),
        }
    }
}

/// What release needs: the container to stop, the workspace, and whether to keep it.
#[derive(Clone, Debug)]
struct DockerTeardown {
    container: String,
    path: PathBuf,
    retention: Retention,
    grace: Duration,
}

#[async_trait]
impl Executor for DockerExecutor {
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError> {
        let RuntimeTarget::Container { image } = &scope.runtime.target else {
            return Err(EnvError::Backend {
                backend: SmolStr::new("docker"),
                operation: SmolStr::new("acquire"),
                message: "this scope does not ask for a container".into(),
            });
        };

        let workspace = self
            .run_dir
            .join("scopes")
            .join(scope.instance.as_str())
            .join("work");
        tokio::fs::create_dir_all(&workspace)
            .await
            .map_err(|e| EnvError::Workspace {
                path: workspace.display().to_string(),
                message: e.to_string(),
            })?;

        self.ensure_image(image).await?;

        let name = self.container_name(&scope.instance).await?;
        // The fence half of the acquire contract (§9): a previous acquisition —
        // a crashed driver's included — left a container under this
        // deterministic name; removing it ends whatever still runs inside and
        // makes its status unreadable. The name carries the run id recorded in
        // the run dir, so a resuming process over the same run dir reaches the
        // crashed run's containers.
        let _ = run_docker(&["rm", "-f", &name]).await;

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
        ];
        for (key, value) in &scope.env {
            create.push("-e".into());
            create.push(format!("{key}={value}"));
        }
        create.push(image.to_string());
        // A long-lived init command, so the container outlives any one step.
        create.extend(["sleep".to_string(), "infinity".to_string()]);

        let refs: Vec<&str> = create.iter().map(String::as_str).collect();
        run_docker(&refs).await?;
        run_docker(&["start", &name]).await?;

        Ok(EnvHandle::new(
            scope.id,
            scope.instance.clone(),
            Arc::new(DockerEnv {
                container: name.clone(),
                workspace: workspace.clone(),
                grace: scope.grace,
            }),
            DockerTeardown {
                container: name,
                path: workspace,
                retention: self.retention,
                grace: scope.grace,
            },
        ))
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let Some(DockerTeardown {
            container,
            path,
            retention,
            grace,
        }) = env.teardown::<DockerTeardown>()
        else {
            return report.problem("docker executor was handed a foreign environment");
        };

        // Container-level kill is exactly right here: everything inside is meant to
        // stop. `stop` sends TERM and waits, then `rm -f` guarantees no leak.
        let grace_secs = grace.as_secs().max(1).to_string();
        let _ = run_docker(&["stop", "-t", &grace_secs, container]).await;
        match run_docker(&["rm", "-f", "-v", container]).await {
            Ok(_) => report = report.released(format!("container {container}")),
            Err(e) => report = report.problem(format!("could not remove {container}: {e}")),
        }

        let workspace = format!("workspace {}", path.display());
        if retention.keeps(outcome) {
            return report.kept(workspace);
        }
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => report = report.released(workspace),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report = report.released(workspace)
            }
            Err(e) => report = report.problem(format!("could not remove {}: {e}", path.display())),
        }
        report
    }
}

struct DockerEnv {
    container: String,
    workspace: PathBuf,
    grace: Duration,
}

#[async_trait]
impl ExecEnv for DockerEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        // The pgid file lives on the bind mount, so the container writes it and the
        // host reads it.
        let token = format!("{}-{}", std::process::id(), next_token());
        let pgid_dir = self.workspace.join(".ci").join("pg");
        tokio::fs::create_dir_all(&pgid_dir)
            .await
            .map_err(|e| EnvError::Workspace {
                path: pgid_dir.display().to_string(),
                message: e.to_string(),
            })?;
        let pgid_host = pgid_dir.join(&token);
        let pgid_in_container = format!("{CONTAINER_WORKSPACE}/.ci/pg/{token}");

        let workdir = match &spec.cwd {
            Some(rel) => format!("{CONTAINER_WORKSPACE}/{}", rel.display()),
            None => CONTAINER_WORKSPACE.to_string(),
        };

        let mut argv: Vec<String> = vec!["exec".into(), "-w".into(), workdir];
        for (key, value) in &spec.env {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
        }
        argv.push(self.container.clone());
        // `setsid` makes the shell a session leader, so its pid is its pgid. The
        // wrapper records that pid, runs the step, and writes the step's exit status
        // beside it — the status file is the source of truth, not the client's code.
        argv.extend([
            "setsid".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            // Both files are written to a temporary name and renamed into place.
            // Rename within a directory is atomic on POSIX, so the host can never
            // read a half-written pgid or status.
            r#"p="$1"; shift; echo $$ > "$p.tmp"; mv "$p.tmp" "$p"; "$@"; s=$?; echo "$s" > "$p.status.tmp"; mv "$p.status.tmp" "$p.status"; exit "$s""#
                .to_string(),
            "sh".to_string(),
            pgid_in_container,
        ]);
        argv.push(spec.program.to_string());
        argv.extend(spec.args.iter().map(|a| a.to_string()));

        let mut command = tokio::process::Command::new("docker");
        command
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| EnvError::Spawn {
            program: SmolStr::new("docker exec"),
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

    // The workspace is bind-mounted from the run directory, so the host filesystem
    // answers for the container. A remote executor would go through its transport.
    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        match tokio::fs::read(self.workspace.join(relative)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(EnvError::Workspace {
                path: relative.display().to_string(),
                message: e.to_string(),
            }),
        }
    }

    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let path = self.workspace.join(relative);
        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(EnvError::Workspace {
                    path: relative.display().to_string(),
                    message: e.to_string(),
                });
            }
        };
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| EnvError::Workspace {
                path: relative.display().to_string(),
                message: e.to_string(),
            })?;
        if bytes.len() > limit {
            return Err(EnvError::Workspace {
                path: relative.display().to_string(),
                message: format!("file exceeds the {limit}-byte read limit"),
            });
        }
        Ok(Some(bytes))
    }

    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| EnvError::Workspace {
                    path: parent.display().to_string(),
                    message: e.to_string(),
                })?;
        }
        tokio::fs::write(&path, contents)
            .await
            .map_err(|e| EnvError::Workspace {
                path: relative.display().to_string(),
                message: e.to_string(),
            })
    }

    fn grace(&self) -> Duration {
        self.grace
    }
}

struct DockerProcess {
    container: String,
    child: tokio::process::Child,
    pgid_file: PathBuf,
    status_file: PathBuf,
    pgid: Option<i32>,
    lines: Option<LineStream>,
}

impl DockerProcess {
    /// The exit status the wrapper recorded, if it got that far.
    async fn recorded_status(&self) -> Option<ExitStatus> {
        let text = tokio::fs::read_to_string(&self.status_file).await.ok()?;
        text.trim().parse::<i32>().ok().map(ExitStatus::code)
    }

    /// Whether the step's process group still has anything in it.
    async fn group_alive(&self) -> bool {
        let Some(pgid) = self.pgid else {
            return false;
        };
        signal_group(&self.container, pgid, "0").await.is_ok()
    }
}

impl DockerProcess {
    /// Read the pgid the step recorded, waiting briefly for it to appear. A cancel
    /// can arrive before the step has written it.
    async fn pgid(&mut self) -> Option<i32> {
        if let Some(pgid) = self.pgid {
            return Some(pgid);
        }
        let deadline = tokio::time::Instant::now() + PGID_WAIT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(text) = tokio::fs::read_to_string(&self.pgid_file).await
                && let Ok(pgid) = text.trim().parse::<i32>()
                && pgid > 0
            {
                self.pgid = Some(pgid);
                return Some(pgid);
            }
            tokio::time::sleep(LIVENESS_POLL).await;
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
        let client = self
            .child
            .wait()
            .await
            .map_err(|e| EnvError::Wait(e.to_string()))?;

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
        if status.is_none() {
            let _ = self.pgid().await;
            loop {
                if let Some(recorded) = self.recorded_status().await {
                    status = Some(recorded);
                    break;
                }
                if !self.group_alive().await {
                    // One last look: the wrapper may have written on its way out.
                    status = self.recorded_status().await;
                    break;
                }
                tokio::time::sleep(LIVENESS_POLL).await;
            }
        }

        let _ = tokio::fs::remove_file(&self.pgid_file).await;
        let _ = tokio::fs::remove_file(&self.status_file).await;
        Ok(status.unwrap_or_else(|| ExitStatus::from(client)))
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        let Some(pgid) = self.pgid().await else {
            // The step never got far enough to record a group. Killing the `docker
            // exec` client is all that is left, and it is enough: there is nothing
            // inside to reach.
            let _ = self.child.start_kill();
            return Ok(());
        };
        // Signal the group INSIDE the container. `docker kill` would hit PID 1 and
        // miss the step entirely.
        let target = format!("-{pgid}");
        let signal = format!("-{}", sig.number());
        let result = run_docker(&["exec", &self.container, "kill", &signal, "--", &target]).await;
        match result {
            Ok(_) => Ok(()),
            // The group is already gone, which is what we wanted anyway.
            Err(_) => Ok(()),
        }
    }
}

/// Signal a process group inside a container.
///
/// Written `kill -SIG -PGID`, with **no `--` separator**. The handoff spells it
/// `kill -<SIG> -- -<PGID>`, which is the POSIX form, but busybox rejects it —
/// `sh: invalid number '--'` — and a rejected signal is a silent one: the step keeps
/// running and the ladder waits out its whole grace period for nothing. Since alpine
/// is the obvious base image, the separator cannot be used. `kill -TERM -123` is
/// understood by busybox ash, dash and bash alike.
///
/// It also goes through `sh -c` rather than as a bare `docker exec … kill`, so this
/// is the shell builtin rather than whichever `kill` binary the image happens to
/// carry.
async fn signal_group(container: &str, pgid: i32, signal: &str) -> Result<(), EnvError> {
    let script = format!("kill -{signal} -{pgid}");
    run_docker(&["exec", container, "sh", "-c", &script])
        .await
        .map(|_| ())
}

fn next_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

async fn run_docker(args: &[&str]) -> Result<String, EnvError> {
    let output = tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| EnvError::Backend {
            backend: SmolStr::new("docker"),
            operation: SmolStr::new(args.first().copied().unwrap_or("docker")),
            message: e.to_string(),
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(EnvError::Backend {
        backend: SmolStr::new("docker"),
        operation: SmolStr::new(args.first().copied().unwrap_or("docker")),
        message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// The run dir's recorded run id, or a fresh one recorded now — on disk before
/// any container carries it, so a crash can never leave a container whose name
/// no later process can rebuild. One executor per run dir is the rule
/// (workspaces would collide otherwise), so write-then-rename is enough: a
/// reader sees the whole id or none.
async fn load_or_record_run_id(run_dir: &Path) -> Result<SmolStr, EnvError> {
    let path = run_dir.join(RUN_ID_FILE);
    let io_error = |path: &Path, e: std::io::Error| EnvError::Workspace {
        path: path.display().to_string(),
        message: e.to_string(),
    };
    match tokio::fs::read_to_string(&path).await {
        Ok(recorded) if !recorded.trim().is_empty() => return Ok(SmolStr::new(recorded.trim())),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error(&path, e)),
    }

    let minted = fresh_run_id();
    tokio::fs::create_dir_all(run_dir)
        .await
        .map_err(|e| io_error(run_dir, e))?;
    let staged = run_dir.join(format!("{RUN_ID_FILE}.tmp"));
    let mut file = tokio::fs::File::create(&staged)
        .await
        .map_err(|e| io_error(&staged, e))?;
    file.write_all(minted.as_bytes())
        .await
        .map_err(|e| io_error(&staged, e))?;
    file.sync_all().await.map_err(|e| io_error(&staged, e))?;
    tokio::fs::rename(&staged, &path)
        .await
        .map_err(|e| io_error(&path, e))?;
    Ok(SmolStr::new(minted))
}

/// Unique across processes and time: two runs on one daemon must never share a
/// container name, or one run's fence would remove the other's containers.
fn fresh_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:x}-{}-{}",
        std::process::id(),
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
