//! The host executor: a workspace directory and real processes on this machine.
//!
//! Implements the [`executor`] interface with nothing in between a step and the
//! operating system. Each step is spawned into its own process group, so a `run:`
//! script that backgrounds children can be signalled — and dies — as a unit.
//!
//! # The sentinel
//!
//! Each spawn starts the group with a tiny supervisor, the **sentinel**: the group
//! leader, which runs the workload as a member of the same group, records the
//! workload's exit status beside the workspace via atomic write (the same wrapper
//! pattern the Docker executor uses for exit-status truth), closes its inherited
//! copies of the stdout/stderr pipes once the workload is running (so the pipes
//! reach EOF when the workload exits), ignores `SIGTERM` so the polite ladder
//! passes through it, and then stays alive until the scope is released.
//!
//! The sentinel is what makes release **safe**. While it lives the group is never
//! empty, so the kernel cannot recycle the pgid; and the executor owns its unreaped
//! handle, so even a sentinel the workload killed pins the id as a zombie. Release
//! sends its one `killpg(SIGKILL)` while the id is still pinned, reaps the
//! sentinel — only then can the kernel recycle the id — and afterwards only
//! **observes** group death by non-signalling means (procfs on Linux, libproc on
//! macOS). No signal is ever sent after the reap frees the id, so a recycled id can
//! at most cause a spurious report entry, never a signal to an innocent. A
//! root-owned member (a step that used `sudo`) may survive the `killpg`; that is a
//! failure-to-kill, reported, categorically different from killing an innocent.
//!
//! `wait` follows a recorded status or group death, whichever comes first — never
//! the sentinel's own exit. An `ESRCH` probe could not do this: the unreaped
//! zombie sentinel keeps the group visible to `kill(-pgid, 0)` until release.
//!
//! # The fence
//!
//! A driver crash kills none of this — the sentinel and workload survive, and the
//! in-memory registry that release would have used dies with the process. So
//! `acquire` **fences prior work** (the [`Executor::acquire`] contract): each
//! acquisition gets a **generation** directory under `groups/`, holding that
//! acquisition's status files and its **group records** — the sentinel's first
//! act is to durably record its pgid there (temp-then-rename), its second to
//! check for the generation's `fenced` marker, and only then does the workload
//! spawn. The fencer writes `fenced` into every prior generation *before*
//! reading its records; whatever the interleaving, a workload that ever starts
//! was discoverable at fence time, and a fenced generation can never start one.
//!
//! A bare persisted pgid is never trusted with a signal: the kernel can recycle
//! it once the whole group is dead, and a pid verified one instant can be
//! recycled the next. So while anything of the group lives, *it* is the killer —
//! a watcher inside the group polls for the marker and kills its own group from
//! inside, where identity is certain; the fencer only waits for the group to
//! drain. A group that never drains — its kill mechanism died with an OOM'd
//! sentinel, or the recorded pgid now belongs to someone else — fails the
//! acquire with the typed [`EnvError::FenceLeaked`], and **no signal is sent**.
//! Per-generation status files are the other half of the same fence: a stale
//! status from a dead run can never be read as the new attempt's exit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    AcquireContext, EnvError, EnvHandle, ExecEnv, Executor, ExitStatus, LineStream, ProcessHandle,
    ProcessSpec, ReleaseReport, Retention, ScopeOutcome, ScopeSpec, Sig,
};
use smol_str::SmolStr;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// How often `wait` re-checks for a recorded status or group death.
pub const LIVENESS_POLL: Duration = Duration::from_millis(25);

/// How long release watches a killed group before reporting it as leaked.
pub const OBSERVE_DEADLINE: Duration = Duration::from_secs(5);

/// How long `acquire`'s fence waits for a discovered live group to drain after
/// the marker is down, before failing with [`EnvError::FenceLeaked`]. Must
/// comfortably exceed the sentinel watcher's poll interval (0.25s in the
/// script).
pub const FENCE_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Directory beside the workspace holding one generation dir per acquisition.
const GROUPS_DIR: &str = "groups";

/// The marker the fencer writes into a generation dir. Its presence means: no
/// workload may start under this generation, and whatever runs must die.
const FENCED_MARKER: &str = "fenced";

/// The shell the sentinel runs under: bash when the machine has it, `/bin/sh`
/// otherwise. Not a style choice — a POSIX `sh` that is dash (Linux's usual
/// `/bin/sh`) *filters out* environment names that are not valid identifiers
/// when it spawns children, and GitHub's contract passes exactly such names
/// (`INPUT_INCLUDE-HIDDEN-FILES`). Bash passes them through. The script itself
/// is plain POSIX either way.
fn sentinel_shell() -> &'static str {
    static SHELL: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    SHELL.get_or_init(|| {
        if std::path::Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        }
    })
}

/// The sentinel, as a [`sentinel_shell`] `-c` script. `$1` is the status file,
/// `$2` the group record, `$3` the generation's fence marker; the rest is the
/// workload's argv.
///
/// Ordering inside the script is load-bearing:
/// - **Publish, then check, then spawn.** The group record (the sentinel's own
///   pid, which is the pgid) is written temp-then-rename before anything else;
///   the fence marker is checked next; only then does the workload spawn. The
///   fencer writes markers before reading records, so a sentinel that misses
///   the marker at its check was published in time to be discovered, and a
///   fenced generation can never start a workload.
/// - The workload is spawned **before** `trap '' TERM`: an ignored disposition is
///   inherited across fork+exec and could never be un-ignored by the workload, so
///   trapping first would break every step that handles `SIGTERM` itself. The
///   window in which a `TERM` could still hit the sentinel is microseconds at
///   spawn time.
/// - `exec >/dev/null 2>&1` drops the sentinel's copies of the stdout/stderr
///   pipes only after the workload holds them, so the pipes reach EOF exactly
///   when the workload (and whatever it spawned) lets go. The fence watcher is
///   spawned after it for the same reason: it must not pin the pipes.
/// - **The group's own members are the killer.** While the workload runs, a
///   watcher subshell polls for the marker and kills the group from inside,
///   where identity is certain; afterwards the pinning loop itself watches. No
///   outside `killpg` ever fires on a bare recorded pgid. (`kill -- -pgid`
///   first; busybox `kill` rejects `--`, hence the fallback form.)
/// - **The watcher lives exactly as long as the workload.** Its loop condition
///   is the workload's liveness and the sentinel reaps it (`kill -9`, since it
///   inherited the ignored TERM) right after `wait` returns — so when a hostile
///   workload murders its sentinel, the group still empties when the workload
///   ends and `wait`'s group-death observation keeps its meaning.
/// - The status is written temp-then-rename, so a reader never sees half a file.
/// - The final loop is what pins the pgid until release, polling the marker at
///   a gentler cadence: a fenced generation with stragglers — or nothing but
///   its pinning sentinel — still dies from inside.
const SENTINEL_SCRIPT: &str = r#"
sf="$1"; gf="$2"; fence="$3"; shift 3
printf '%s\n' "$$" > "$gf.tmp" && mv "$gf.tmp" "$gf"
if [ -e "$fence" ]; then exit 0; fi
"$@" &
w=$!
trap '' TERM
exec >/dev/null 2>&1
( while kill -0 "$w" 2>/dev/null; do
    if [ -e "$fence" ]; then kill -KILL -- "-$$" 2>/dev/null || kill -KILL "-$$"; fi
    sleep 0.25
  done ) &
watch=$!
wait "$w"
s=$?
kill -9 "$watch" 2>/dev/null
echo "$s" > "$sf.tmp" && mv "$sf.tmp" "$sf"
while :; do
  if [ -e "$fence" ]; then kill -KILL -- "-$$" 2>/dev/null || kill -KILL "-$$"; fi
  sleep 1
done
"#;

/// Runs steps as processes on this machine.
pub struct HostExecutor {
    run_dir: PathBuf,
    retention: Retention,
    fence_drain: Duration,
}

impl HostExecutor {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
            retention: Retention::default(),
            fence_drain: FENCE_DRAIN_DEADLINE,
        }
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// Override how long the fence waits for a discovered group to drain.
    pub fn with_fence_drain(mut self, deadline: Duration) -> Self {
        self.fence_drain = deadline;
        self
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn workspace_for(&self, instance: &str) -> PathBuf {
        self.run_dir.join("scopes").join(instance).join("work")
    }
}

/// One spawn's process group, pinned from spawn to release.
///
/// The `sentinel` handle is deliberately never awaited before release: a reaped —
/// or dropped, tokio's orphan reaper counts — sentinel would free the pgid for
/// kernel reuse, and release's `killpg` could then reach an innocent.
#[derive(Debug)]
struct PinnedGroup {
    pgid: i32,
    sentinel: tokio::process::Child,
}

/// What release needs: the workspace, whether to keep it, and every process group
/// still pinned.
#[derive(Clone, Debug)]
struct HostTeardown {
    path: PathBuf,
    retention: Retention,
    groups: Arc<Mutex<Vec<PinnedGroup>>>,
}

#[async_trait]
impl Executor for HostExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        _ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        // This executor is deliberately Docker-free; a scope that declares
        // sidecar services needs a composition that can realize them.
        // `LocalExecutor` satisfies this guard by realizing the services
        // itself and handing this executor a spec with none.
        if !scope.services.is_empty() {
            return Err(EnvError::Backend {
                backend: SmolStr::new("host"),
                operation: SmolStr::new("acquire"),
                message: "this scope declares service containers, which the host executor \
                          cannot realize; use the local executor"
                    .into(),
            });
        }
        let workspace = self.workspace_for(&scope.instance);
        tokio::fs::create_dir_all(&workspace)
            .await
            .map_err(|e| EnvError::Workspace {
                path: workspace.display().to_string(),
                message: e.to_string(),
            })?;
        // Group records and status files live beside the workspace, not in it,
        // so steps never see them and workspace teardown never races the
        // sentinel's rename — one generation dir per acquisition, which is what
        // isolates a dead run's status files from this one's.
        let groups_root = workspace
            .parent()
            .map(|p| p.join(GROUPS_DIR))
            .unwrap_or_else(|| workspace.join(format!(".{GROUPS_DIR}")));
        tokio::fs::create_dir_all(&groups_root)
            .await
            .map_err(|e| EnvError::Workspace {
                path: groups_root.display().to_string(),
                message: e.to_string(),
            })?;
        fence_prior_generations(&groups_root, self.fence_drain).await?;
        let gen_dir = groups_root.join(fresh_generation_id());
        tokio::fs::create_dir_all(&gen_dir)
            .await
            .map_err(|e| EnvError::Workspace {
                path: gen_dir.display().to_string(),
                message: e.to_string(),
            })?;
        let groups = Arc::new(Mutex::new(Vec::new()));
        Ok(EnvHandle::new(
            scope.id,
            scope.instance.clone(),
            Arc::new(HostEnv {
                workspace: workspace.clone(),
                workspace_str: workspace.display().to_string(),
                gen_dir,
                env: scope.env.clone(),
                grace: scope.grace,
                groups: Arc::clone(&groups),
                seq: AtomicU64::new(0),
            }),
            HostTeardown {
                path: workspace,
                retention: self.retention,
                groups,
            },
        ))
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let mut report = ReleaseReport::default();
        let Some(teardown) = env.teardown::<HostTeardown>() else {
            return report.problem("host executor was handed a foreign environment");
        };
        let (path, retention) = (teardown.path.clone(), teardown.retention);
        let groups: Vec<PinnedGroup> = teardown
            .groups
            .lock()
            .map(|mut held| held.drain(..).collect())
            .unwrap_or_default();
        drop(env);

        // Process groups first, workspace second — a straggler may still be
        // writing into it. Groups are cleaned whatever the retention policy says
        // about the workspace: retention keeps files, never processes.
        //
        // One SIGKILL per group, each sent while its sentinel — alive or zombie —
        // still pins the id, so none can reach a recycled group. All groups are
        // killed before any is observed, so a straggler in one never delays the
        // kill of the next.
        //
        // SAFETY: killpg takes a pgid and a signal number and has no memory
        // effects. Errors are ignored: an unsignallable member (root-owned) is
        // caught by the observation below.
        for group in &groups {
            unsafe {
                libc::killpg(group.pgid, libc::SIGKILL);
            }
        }
        // Reap the sentinels. Only after this can the kernel recycle the ids,
        // which is why nothing below ever signals a group again.
        let mut pgids = Vec::new();
        for mut group in groups {
            let _ = group.sentinel.wait().await;
            pgids.push(group.pgid);
        }
        // Observe group death — non-signalling, all groups under one shared
        // deadline. A group that outlives it is a leak to report.
        let deadline = tokio::time::Instant::now() + OBSERVE_DEADLINE;
        let leaked = await_drain(pgids.clone(), deadline).await;
        for pgid in pgids {
            report = if leaked.contains(&pgid) {
                report.problem(format!("process group {pgid} outlived release"))
            } else {
                report.released(format!("process group {pgid}"))
            };
        }

        let workspace = format!("workspace {}", path.display());
        if retention.keeps(outcome) {
            return report.kept(workspace);
        }
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => report = report.released(workspace),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report = report.released(workspace)
            }
            Err(e) => report = report.problem(format!("could not remove {}: {e}", path.display())),
        }
        report
    }
}

struct HostEnv {
    workspace: PathBuf,
    workspace_str: String,
    /// This acquisition's generation dir: its status files, group records, and
    /// fence marker.
    gen_dir: PathBuf,
    env: BTreeMap<SmolStr, SmolStr>,
    grace: Duration,
    groups: Arc<Mutex<Vec<PinnedGroup>>>,
    seq: AtomicU64,
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

        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let status_file = self.gen_dir.join(format!("{n}.status"));
        let group_file = self.gen_dir.join(format!("{n}.group"));
        let fence = self.gen_dir.join(FENCED_MARKER);

        let mut command = tokio::process::Command::new(sentinel_shell());
        command
            .arg("-c")
            .arg(SENTINEL_SCRIPT)
            .arg("petri-sentinel") // $0
            .arg(&status_file) // $1
            .arg(&group_file) // $2
            .arg(&fence) // $3
            .arg(spec.program.as_str())
            .args(spec.args.iter().map(|a| a.as_str()))
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, value) in self.env.iter().chain(spec.env.iter()) {
            command.env(key.as_str(), value.as_str());
        }
        // Its own process group, led by the sentinel, so the whole tree can be
        // signalled — and observed — as a unit. No `kill_on_drop`: cleanup
        // ownership lives wholly with release, and a drop-kill would reach only
        // the leader anyway.
        command.process_group(0);

        let mut child = command.spawn().map_err(|e| EnvError::Spawn {
            program: spec.program.clone(),
            message: e.to_string(),
        })?;
        let pgid = child.id().ok_or_else(|| EnvError::Spawn {
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

        // The registry owns the sentinel from here to release.
        if let Ok(mut groups) = self.groups.lock() {
            groups.push(PinnedGroup {
                pgid,
                sentinel: child,
            });
        }

        Ok(Box::new(HostProcess {
            pgid,
            status_file,
            status: None,
            lines: Some(rx),
        }))
    }

    fn workspace_path(&self) -> &str {
        &self.workspace_str
    }

    fn ambient_env(&self, name: &str) -> Option<String> {
        // The scope env over the inherited process env — the same order
        // `spawn` applies them.
        self.env
            .get(name)
            .map(|v| v.to_string())
            .or_else(|| std::env::var(name).ok())
    }

    fn shares_host_filesystem(&self) -> bool {
        true
    }

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

struct HostProcess {
    /// The sentinel's pid, which is the group id. The sentinel — not the workload
    /// — is the group leader.
    pgid: i32,
    status_file: PathBuf,
    /// The first resolution wins and is cached: `wait` may be called again after
    /// each rung of the cancellation ladder.
    status: Option<ExitStatus>,
    lines: Option<LineStream>,
}

#[async_trait]
impl ProcessHandle for HostProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    /// A recorded status or group death, whichever comes first — never the
    /// sentinel's own exit, and never a `waitpid` on it (release owns the reap).
    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        loop {
            if let Some(code) = recorded_status(&self.status_file).await {
                let status = decode_status(code);
                self.status = Some(status);
                return Ok(status);
            }
            if !group_is_live(self.pgid) {
                // The sentinel died without recording — our KILL, or a hostile
                // workload's. One more read covers a rename that landed between
                // the two checks.
                let status = match recorded_status(&self.status_file).await {
                    Some(code) => decode_status(code),
                    None => ExitStatus::signalled(libc::SIGKILL),
                };
                self.status = Some(status);
                return Ok(status);
            }
            tokio::time::sleep(LIVENESS_POLL).await;
        }
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        // killpg, never kill: the group is the unit. The sentinel ignores TERM, so
        // the polite rung passes through it to the workload.
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

/// A generation id unique across acquisitions and process restarts.
fn fresh_generation_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "g{nanos:x}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The fence half of the [`Executor::acquire`] contract, over every prior
/// generation under `groups/`.
///
/// Ordering closes the publication race: the `fenced` marker is written into a
/// generation **before** its group records are read, and the sentinel publishes
/// before checking the marker — so a workload that ever starts was discoverable
/// here, and a fenced generation can never start one. While anything of a
/// discovered group lives, the group's own in-group watcher is the killer; this
/// side only waits for the group to drain. Nothing here ever signals: a
/// recorded pgid can be recycled to an innocent, so a group that never drains
/// fails the acquire with [`EnvError::FenceLeaked`] instead. Fencing an
/// already-fenced (or already-dead) generation is a no-op, which is what makes
/// the fence idempotent.
async fn fence_prior_generations(groups_root: &Path, drain: Duration) -> Result<(), EnvError> {
    let Ok(entries) = std::fs::read_dir(groups_root) else {
        return Ok(());
    };
    let mut discovered: Vec<(SmolStr, i32)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let gen_dir = entry.path();
        let generation = SmolStr::new(entry.file_name().to_string_lossy());
        // The marker lands before any record is read — the publication race
        // closes on this ordering.
        let marker = gen_dir.join(FENCED_MARKER);
        if let Err(e) = std::fs::write(&marker, b"") {
            return Err(EnvError::Workspace {
                path: marker.display().to_string(),
                message: e.to_string(),
            });
        }
        let Ok(records) = std::fs::read_dir(&gen_dir) else {
            continue;
        };
        for record in records.flatten() {
            let path = record.path();
            if path.extension().is_none_or(|ext| ext != "group") {
                continue;
            }
            let Some(pgid) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            else {
                continue;
            };
            discovered.push((generation.clone(), pgid));
        }
    }

    let deadline = tokio::time::Instant::now() + drain;
    let pgids = discovered.iter().map(|(_, pgid)| *pgid).collect();
    let leaked = await_drain(pgids, deadline).await;
    let Some((generation, pgid)) = discovered
        .into_iter()
        .find(|(_, pgid)| leaked.contains(pgid))
    else {
        return Ok(());
    };
    let why = if sentinel_is_live(pgid) {
        "whatever leads it never honored the marker, so it is not ours to kill"
    } else {
        "its sentinel is gone, so nothing can kill it from inside"
    };
    Err(EnvError::FenceLeaked {
        generation,
        detail: format!(
            "process group {pgid} did not drain ({why}); no signal is ever sent \
             to a bare recorded pgid — end the group and retry"
        ),
    })
}

/// Wait for process groups to drain without signalling any of them: the groups
/// still alive at `deadline`, empty when all are gone. Members a KILL reached
/// are zombies at worst (init reaps them); only live members count.
async fn await_drain(mut pgids: Vec<i32>, deadline: tokio::time::Instant) -> Vec<i32> {
    loop {
        pgids.retain(|&pgid| live_group_members(pgid) > 0);
        if pgids.is_empty() || tokio::time::Instant::now() >= deadline {
            return pgids;
        }
        tokio::time::sleep(LIVENESS_POLL).await;
    }
}

async fn recorded_status(path: &Path) -> Option<i32> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    text.trim().parse::<i32>().ok()
}

/// The shell reports a signalled child as `128 + N`; translate that back so a
/// foreign signal still reads as one (`Failure { class: "signal:N" }`, §10). A
/// process that deliberately exits with such a code is indistinguishable — the
/// same trade the Docker wrapper's status file makes.
fn decode_status(code: i32) -> ExitStatus {
    if (129..=192).contains(&code) {
        ExitStatus::signalled(code - 128)
    } else {
        ExitStatus::code(code)
    }
}

/// Whether the group still has a live member, probed cheaply through the
/// sentinel first. `wait` runs this every poll tick for a step's whole natural
/// duration, and while the executor holds the sentinel unreaped its pid — the
/// pgid — cannot be recycled, so the probe is the sentinel itself: alive means
/// the group is alive without enumerating it. Only a dead or zombie sentinel —
/// our KILL, or a hostile workload's — makes the full listing necessary.
fn group_is_live(pgid: i32) -> bool {
    sentinel_is_live(pgid) || live_group_members(pgid) > 0
}

/// One `/proc/<pgid>/stat` read: the sentinel, live and still leading the group.
#[cfg(target_os = "linux")]
fn sentinel_is_live(pgid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pgid}/stat"))
        .is_ok_and(|stat| stat_is_live_in_group(&stat, pgid))
}

/// One libproc query: the sentinel, not yet a zombie.
#[cfg(target_os = "macos")]
fn sentinel_is_live(pgid: i32) -> bool {
    unsafe { is_live(pgid) }
}

/// How many *live* processes remain in the group. Non-signalling, and zombies do
/// not count: they cannot run, and the unreaped sentinel is deliberately one.
#[cfg(target_os = "linux")]
fn live_group_members(pgid: i32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("stat"))
                .is_ok_and(|stat| stat_is_live_in_group(&stat, pgid))
        })
        .count()
}

/// Parse one `/proc/<pid>/stat` line: `pid (comm) state ppid pgrp ...`. The comm
/// can hold spaces and parentheses, so the split is after the *last* `)`.
#[cfg(target_os = "linux")]
fn stat_is_live_in_group(stat: &str, pgid: i32) -> bool {
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let mut fields = rest.split_whitespace();
    let Some(state) = fields.next() else {
        return false;
    };
    let Some(group) = fields.nth(1).and_then(|s| s.parse::<i32>().ok()) else {
        return false;
    };
    group == pgid && state != "Z"
}

/// libproc's process listing by group, each member checked for zombie state.
#[cfg(target_os = "macos")]
fn live_group_members(pgid: i32) -> usize {
    const PROC_PGRP_ONLY: u32 = 2;
    unsafe {
        let bytes = libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, std::ptr::null_mut(), 0);
        if bytes <= 0 {
            return 0;
        }
        let mut pids = vec![0i32; bytes as usize / size_of::<i32>() + 8];
        let bytes = libc::proc_listpids(
            PROC_PGRP_ONLY,
            pgid as u32,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        );
        if bytes <= 0 {
            return 0;
        }
        pids.truncate(bytes as usize / size_of::<i32>());
        pids.into_iter()
            .filter(|&pid| pid > 0 && is_live(pid))
            .count()
    }
}

#[cfg(target_os = "macos")]
unsafe fn is_live(pid: i32) -> bool {
    // sys/proc.h: SZOMB. libproc reports it through proc_bsdinfo.pbi_status.
    const SZOMB: u32 = 5;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let got =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    // A process that vanished between the listing and this query is not live.
    got == size && info.pbi_status != SZOMB
}
