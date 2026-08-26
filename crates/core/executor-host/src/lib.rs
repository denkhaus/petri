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
//! at worst cause a spurious report entry, never a signal to an innocent. A
//! root-owned member (a step that used `sudo`) may survive the `killpg`; that is a
//! failure-to-kill, reported, categorically different from killing an innocent.
//!
//! `wait` follows a recorded status or group death, whichever comes first — never
//! the sentinel's own exit. An `ESRCH` probe could not do this: the unreaped
//! zombie sentinel keeps the group visible to `kill(-pgid, 0)` until release.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::pump;
use executor::{
    EnvError, EnvHandle, ExecEnv, Executor, ExitStatus, LineStream, ProcessHandle, ProcessSpec,
    ReleaseReport, Retention, ScopeOutcome, ScopeSpec, Sig,
};
use smol_str::SmolStr;
use tokio::sync::mpsc;

/// How often `wait` re-checks for a recorded status or group death.
pub const LIVENESS_POLL: Duration = Duration::from_millis(25);

/// How long release watches a killed group before reporting it as leaked.
pub const OBSERVE_DEADLINE: Duration = Duration::from_secs(5);

/// The sentinel, as a `/bin/sh -c` script. `$1` is the status file; the rest is
/// the workload's argv.
///
/// Ordering inside the script is load-bearing:
/// - The workload is spawned **before** `trap '' TERM`: an ignored disposition is
///   inherited across fork+exec and could never be un-ignored by the workload, so
///   trapping first would break every step that handles `SIGTERM` itself. The
///   window in which a `TERM` could still hit the sentinel is microseconds at
///   spawn time.
/// - `exec >/dev/null 2>&1` drops the sentinel's copies of the stdout/stderr
///   pipes only after the workload holds them, so the pipes reach EOF exactly
///   when the workload (and whatever it spawned) lets go.
/// - The status is written temp-then-rename, so a reader never sees half a file.
/// - The final loop is what pins the pgid until release.
const SENTINEL_SCRIPT: &str = r#"
sf="$1"; shift
"$@" &
w=$!
trap '' TERM
exec >/dev/null 2>&1
wait "$w"
s=$?
echo "$s" > "$sf.tmp" && mv "$sf.tmp" "$sf"
while :; do sleep 300; done
"#;

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
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError> {
        let workspace = self.workspace_for(&scope.instance);
        tokio::fs::create_dir_all(&workspace)
            .await
            .map_err(|e| EnvError::Workspace {
                path: workspace.display().to_string(),
                message: e.to_string(),
            })?;
        // Status files live beside the workspace, not in it, so steps never see
        // them and workspace teardown never races the sentinel's rename.
        let status_dir = workspace
            .parent()
            .map(|p| p.join("groups"))
            .unwrap_or_else(|| workspace.join(".groups"));
        tokio::fs::create_dir_all(&status_dir)
            .await
            .map_err(|e| EnvError::Workspace {
                path: status_dir.display().to_string(),
                message: e.to_string(),
            })?;
        let groups = Arc::new(Mutex::new(Vec::new()));
        Ok(EnvHandle::new(
            scope.id,
            scope.instance.clone(),
            Arc::new(HostEnv {
                workspace: workspace.clone(),
                workspace_str: workspace.display().to_string(),
                status_dir,
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
        for mut group in groups {
            // One SIGKILL, sent while the sentinel — alive or zombie — still pins
            // the id, so it cannot reach a recycled group.
            //
            // SAFETY: killpg takes a pgid and a signal number and has no memory
            // effects. Errors are ignored: an unsignallable member (root-owned)
            // is caught by the observation below.
            unsafe {
                libc::killpg(group.pgid, libc::SIGKILL);
            }
            // Reap the sentinel. Only after this can the kernel recycle the id,
            // which is why nothing below ever signals the group again.
            let _ = group.sentinel.wait().await;
            // Observe group death — non-signalling, bounded. Members our KILL
            // reached are zombies at worst (init reaps them); only live members
            // count, and one that outlives the deadline is a leak to report.
            let deadline = tokio::time::Instant::now() + OBSERVE_DEADLINE;
            loop {
                if live_group_members(group.pgid) == 0 {
                    report = report.released(format!("process group {}", group.pgid));
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    report = report.problem(format!(
                        "process group {} outlived release",
                        group.pgid
                    ));
                    break;
                }
                tokio::time::sleep(LIVENESS_POLL).await;
            }
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
    status_dir: PathBuf,
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
        let status_file = self.status_dir.join(format!("{n}.status"));

        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(SENTINEL_SCRIPT)
            .arg("petri-sentinel") // $0
            .arg(&status_file) // $1
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
            if live_group_members(self.pgid) == 0 {
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
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    // A process that vanished between the listing and this query is not live.
    got == size && info.pbi_status != SZOMB
}
