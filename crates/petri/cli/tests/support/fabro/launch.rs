//! Launch the shipped `petri` binary in an isolated environment and read the
//! run back.
//!
//! The child sees none of the developer's environment: no provider
//! credentials, no shell configuration, a fresh `HOME`, and a catalog layer
//! that points every provider a case uses at a loopback twin. A provider the
//! case did not redirect is not enabled at all, so a workflow that names one
//! fails before any live endpoint is reached.
//!
//! Every launch has a deadline. On expiry the child's process group is
//! killed and reaped, so a hung run fails the test instead of hanging
//! Nextest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use std::{env, fs, process};

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout};

use super::inspect;
use super::twins::{Provider, Twin};

/// How long one `petri run` may take before the harness kills it.
pub(crate) const RUN_DEADLINE: Duration = Duration::from_secs(120);

/// One case's private directory tree: workflow, run dir, home, store.
pub(crate) struct Case {
    pub(crate) root:       PathBuf,
    pub(crate) run_dir:    PathBuf,
    /// The fake credential this case hands every twin: its namespace.
    pub(crate) credential: String,
    layers:                Vec<String>,
    providers:             Vec<&'static str>,
    plugin_link:           PathBuf,
}

impl Case {
    /// A fresh case under the system temp dir. `label` names it; the process
    /// id and a counter keep concurrent cases apart.
    pub(crate) fn new(label: &str) -> Self {
        // Kept after the test as failure evidence; the temp dir ages out.
        let root = env::temp_dir().join("petri-tests").join(format!(
            "fabro-blackbox-{label}-{}-{}",
            process::id(),
            testkit::unique_id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("case root");
        for sub in ["home", "store", "cache", "plugins", "workflow"] {
            fs::create_dir_all(root.join(sub)).expect("case subdirectory");
        }
        let plugin = env::var_os("PETRI_SANDBOX_HOST_PLUGIN")
            .map(PathBuf::from)
            .expect("PETRI_SANDBOX_HOST_PLUGIN names the host sandbox plugin");
        // A per-case path for the plugin executable, so a leaked plugin
        // process is attributable to this case by its command line.
        let plugin_link = root.join("plugins").join("sandbox-driver-host");
        link_plugin(&plugin, &plugin_link);
        Self {
            credential: format!("fake-{label}-{}", testkit::unique_id()),
            run_dir: root.join("run"),
            root,
            layers: Vec::new(),
            providers: Vec::new(),
            plugin_link,
        }
    }

    /// Point `provider` at `twin` for this case's runs.
    pub(crate) fn redirect(&mut self, twin: &Twin) {
        self.layers.push(twin.catalog_layer());
        self.providers.push(twin.provider.id());
    }

    /// Point `provider` at a base URL nothing listens on.
    pub(crate) fn redirect_to_nothing(&mut self, provider: Provider, base_url: &str) {
        self.layers.push(format!(
            "schema_version = 1\n[providers.{}]\nbase_url = {base_url:?}\n",
            provider.id()
        ));
        self.providers.push(provider.id());
    }

    /// Write the workflow (and optional `workflow.toml`) this case runs.
    pub(crate) fn workflow(&self, dot: &str, workflow_toml: Option<&str>) -> PathBuf {
        let dir = self.root.join("workflow");
        let path = dir.join("case.fabro");
        fs::write(&path, dot).expect("write the workflow");
        if let Some(toml) = workflow_toml {
            fs::write(dir.join("workflow.toml"), toml).expect("write workflow.toml");
        }
        path
    }

    /// The host workspace of the root invocation's first scope.
    pub(crate) fn workspace(&self) -> PathBuf {
        self.run_dir
            .join("scopes")
            .join("invocation-0-scope-0")
            .join("work")
    }

    /// Run `petri run <args…> <workflow>` to completion, or kill it at the
    /// deadline.
    pub(crate) async fn run(&self, workflow: &Path, args: &[&str]) -> Finished {
        self.run_with(workflow, args, Launch::default()).await
    }

    /// [`Case::run`] with piped terminal input and an optional interrupt.
    pub(crate) async fn run_with(
        &self,
        workflow: &Path,
        args: &[&str],
        launch: Launch,
    ) -> Finished {
        let catalog = self.root.join("catalog.toml");
        fs::write(&catalog, self.layers.join("\n")).expect("write the catalog layer");
        let path = launch
            .path
            .clone()
            .or_else(|| env::var("PATH").ok())
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        let mut command = Command::new(env!("CARGO_BIN_EXE_petri"));
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            // The system temp dir, not a per-case one: the sandbox plugin
            // binds a Unix socket under it, and a long path exceeds the
            // socket path limit.
            .env("TMPDIR", env::temp_dir())
            .env("PETRI_STORE", self.root.join("store"))
            .env("PETRI_CACHE_DIR", self.root.join("cache"))
            .env("PETRI_LLM_CATALOG", &catalog)
            .env("PETRI_LLM_PROVIDERS", self.providers.join(","))
            .env("PETRI_SANDBOX_HOST_PLUGIN", &self.plugin_link)
            .env("PETRI_SANDBOX_PLUGIN_DEV", "1")
            .env("PETRI_LOG", "warn");
        for (key, value) in &launch.env {
            command.env(key, value);
        }
        for provider in &self.providers {
            let variable = match *provider {
                "openai" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                "openrouter" => "OPENROUTER_API_KEY",
                other => panic!("no credential variable for provider `{other}`"),
            };
            command.env(variable, &self.credential);
        }
        command
            .arg("run")
            .args(args)
            .arg("--run-dir")
            .arg(&self.run_dir)
            .arg(workflow)
            .stdin(if launch.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().expect("petri starts");
        let pid = child.id().expect("a running child has a pid");
        if let Some(text) = launch.stdin {
            let mut stdin = child.stdin.take().expect("piped stdin");
            tokio::spawn(async move {
                let _ = stdin.write_all(text.as_bytes()).await;
                // Kept open: EOF on the terminal is its own failure mode, and
                // a case that wants it passes an empty script and closes.
                if launch.close_stdin {
                    drop(stdin);
                } else {
                    sleep(RUN_DEADLINE).await;
                }
            });
        }
        let interrupt = launch.interrupt_when.map(|marker| {
            tokio::spawn(async move {
                let deadline = Instant::now() + RUN_DEADLINE;
                while !marker.exists() {
                    assert!(
                        Instant::now() < deadline,
                        "the interrupt marker {} never appeared",
                        marker.display()
                    );
                    sleep(Duration::from_millis(50)).await;
                }
                #[cfg(unix)]
                {
                    let _ = Command::new("kill")
                        .args(["-INT", &pid.to_string()])
                        .stdin(Stdio::null())
                        .status()
                        .await;
                }
            })
        });
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let drain = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            a.expect("read stdout");
            b.expect("read stderr");
            (out, err)
        };
        let waited = timeout(RUN_DEADLINE, async {
            let ((out, err), status) = tokio::join!(drain, child.wait());
            (out, err, status.expect("wait for petri"))
        })
        .await;
        if let Some(interrupt) = interrupt {
            interrupt.abort();
        }
        let (stdout, stderr, status, timed_out) = if let Ok((out, err, status)) = waited {
            (out, err, Some(status), false)
        } else {
            kill_group(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            (Vec::new(), Vec::new(), None, true)
        };
        Finished {
            case_root: self.root.clone(),
            run_dir: self.run_dir.clone(),
            plugin_link: self.plugin_link.clone(),
            code: status.and_then(|status| status.code()),
            timed_out,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }
}

#[cfg(unix)]
fn link_plugin(plugin: &Path, link: &Path) {
    use std::os::unix::fs::symlink;
    symlink(plugin, link).expect("link the plugin");
}

#[cfg(not(unix))]
fn link_plugin(plugin: &Path, link: &Path) {
    fs::copy(plugin, link).expect("copy the plugin");
}

/// How a launch differs from the plain `run`.
#[derive(Default)]
pub(crate) struct Launch {
    /// Text written to the child's stdin, for `--interactive`.
    pub(crate) stdin:          Option<String>,
    /// Close stdin after writing it (EOF), instead of holding it open.
    pub(crate) close_stdin:    bool,
    /// Send SIGINT once this file exists: how a case cancels a run the way a
    /// person at the terminal does.
    pub(crate) interrupt_when: Option<PathBuf>,
    /// The child's `PATH`. Defaults to the harness's own.
    pub(crate) path:           Option<String>,
    /// Extra environment variables for the child: what a case hands the run
    /// beyond the isolated baseline, such as a `PETRI_SECRET_*` value.
    pub(crate) env:            Vec<(String, String)>,
}

/// A `PATH` with an empty directory in front and only the system binaries
/// behind it, so no `fabro` resolves. `None` when a `fabro` lives in one of
/// those system directories, which the harness cannot sanitize.
pub(crate) fn sanitized_path(root: &Path) -> Option<String> {
    let empty = root.join("empty-bin");
    fs::create_dir_all(&empty).expect("the empty bin dir");
    let system = ["/usr/bin", "/bin"];
    if system.iter().any(|d| Path::new(d).join("fabro").exists()) {
        return None;
    }
    Some(format!("{}:{}", empty.display(), system.join(":")))
}

/// Kill a child's whole process group, then reap what `kill_on_drop` cannot.
async fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = pid;
}

/// What a launch left behind.
pub(crate) struct Finished {
    pub(crate) case_root: PathBuf,
    pub(crate) run_dir:   PathBuf,
    plugin_link:          PathBuf,
    /// The exit code, or `None` when the child was killed (deadline or
    /// signal).
    pub(crate) code:      Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) stdout:    String,
    pub(crate) stderr:    String,
}

impl Finished {
    /// Fail the test unless the run exited with `code`.
    pub(crate) fn assert_code(&self, code: i32) {
        assert!(!self.timed_out, "petri run exceeded {RUN_DEADLINE:?}");
        assert_eq!(
            self.code,
            Some(code),
            "exit code\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
    }

    /// The final run status line, `run: success` and the like.
    pub(crate) fn status_line(&self) -> Option<&str> {
        self.stderr
            .lines()
            .find(|line| line.starts_with("run: "))
            .map(|line| &line[5..])
    }

    /// The interview receipt the run wrote.
    pub(crate) fn receipt(&self) -> Value {
        let path = self.run_dir.join("interviews.json");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_str(&text).expect("interviews.json is JSON")
    }

    /// The `petri inspect --json` document for this run.
    pub(crate) fn inspect(&self) -> Value {
        inspect::inspect(&self.run_dir)
    }

    /// The final run context of the root invocation, read through
    /// `petri inspect`.
    pub(crate) fn final_context(&self) -> BTreeMap<String, Value> {
        let document = self.inspect();
        inspect::root_context(&document)
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .expect("the root invocation finished with a context")
    }

    /// The node names that finished, in completion order, from the CLI's
    /// own summary lines (`  <status> <node>`).
    pub(crate) fn finished_nodes(&self) -> Vec<(String, String)> {
        self.stderr
            .lines()
            .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
            .filter_map(|line| {
                let mut parts = line.trim().splitn(2, ' ');
                Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
            })
            .collect()
    }

    /// The retained workspace paths the run reported.
    pub(crate) fn reported_workspaces(&self) -> Vec<PathBuf> {
        self.stderr
            .lines()
            .filter_map(|line| line.strip_prefix("workspace: "))
            .map(|path| PathBuf::from(path.trim()))
            .collect()
    }

    /// Every echoed step line, `[node#firing] text`, as (node, text).
    pub(crate) fn echoed(&self) -> Vec<(String, String)> {
        self.stderr
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix('[')?;
                let end = rest.find("] ")?;
                let tag = &rest[..end];
                let node = tag.split('#').next()?;
                Some((node.to_owned(), rest[end + 2..].to_owned()))
            })
            .collect()
    }

    /// Fail the test when a process launched for this case is still alive:
    /// the plugin (by its per-case path) or anything naming the run dir.
    pub(crate) async fn assert_no_leaked_processes(&self) {
        for needle in [
            self.plugin_link.to_string_lossy().into_owned(),
            self.run_dir.to_string_lossy().into_owned(),
        ] {
            let output = Command::new("pgrep")
                .args(["-f", &needle])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("pgrep runs");
            let pids = String::from_utf8_lossy(&output.stdout);
            let pids: Vec<&str> = pids
                .lines()
                .map(str::trim)
                .filter(|pid| !pid.is_empty())
                .collect();
            assert!(
                pids.is_empty(),
                "processes still running for `{needle}`: {pids:?}"
            );
        }
    }
}
