//! The pinned Fabro adapter for the differential matrix (black box phase 5).
//!
//! It provisions the reference binary, starts a private Fabro server per
//! case, runs the same bundle with the same inputs, twins and interview
//! script that the Petri side runs, and reads the run back through Fabro's
//! public surfaces only: `fabro validate`, `fabro run`, the questions API,
//! `fabro events`, `fabro dump` and `GET /api/v1/runs/{id}/state`.
//!
//! Nothing here links a Fabro crate. The binary is the one
//! `scripts/fabro-provision.sh` built from the fetched corpus at the pin in
//! `crates/fabro/corpus-pin.txt`; a binary whose version string does not
//! carry the pin's short SHA is refused, and a `fabro` on `PATH` is never
//! used. Every server has its own home, storage, port and credentials, and
//! is killed with its process group when the case ends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use std::{env, fs, process};

use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};

use super::twins::{Provider, Twin};

/// The dev token the private server accepts. It is a fixed placeholder: the
/// server binds loopback only and lives for one case.
const DEV_TOKEN: &str =
    "fabro_dev_abababababababababababababababababababababababababababababababab";
const SESSION_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Set to `1` to fail instead of skip when the pinned binary is missing.
pub(crate) const REQUIRE_ENV: &str = "PETRI_REQUIRE_FABRO_BINARY";

/// How long one Fabro run may take before the adapter cancels it.
pub(crate) const RUN_DEADLINE: Duration = Duration::from_secs(180);

/// The repository root, from this crate's manifest directory.
pub(crate) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("the repository root exists")
}

/// The pinned Fabro commit from `crates/fabro/corpus-pin.txt`.
pub(crate) fn pinned_commit() -> String {
    let path = repo_root().join("crates/fabro/corpus-pin.txt");
    let text = fs::read_to_string(&path).expect("the Fabro pin file is tracked");
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_whitespace().next().unwrap_or(line).to_owned())
        .expect("the pin file names a commit")
}

/// The pinned Fabro binary: its path, version string and commit.
#[derive(Clone, Debug)]
pub(crate) struct FabroBinary {
    pub(crate) path:    PathBuf,
    pub(crate) version: String,
    pub(crate) commit:  String,
}

impl FabroBinary {
    /// The provisioned binary, or `None` when none exists (the case then
    /// runs Petri against the committed reference). With
    /// `PETRI_REQUIRE_FABRO_BINARY=1` a missing binary fails. A binary that
    /// exists but does not report the pin always fails: an unpinned
    /// reference is never a baseline.
    pub(crate) fn provisioned() -> Option<Self> {
        let commit = pinned_commit();
        let path = env::var_os("FABRO_BIN").map_or_else(
            || repo_root().join("crates/fabro/corpus/fabro-target/debug/fabro"),
            PathBuf::from,
        );
        if !path.is_file() {
            assert!(
                env::var(REQUIRE_ENV).is_ok_and(|value| value == "1"),
                "no pinned fabro binary at {}; run scripts/fabro-provision.sh",
                path.display()
            );
            eprintln!(
                "skipping the pinned Fabro side: no binary at {} (scripts/fabro-provision.sh builds \
                 it; {REQUIRE_ENV}=1 fails instead)",
                path.display()
            );
            return None;
        }
        let output = process::Command::new(&path)
            .arg("--version")
            .env_clear()
            .env("PATH", system_path())
            .env("FABRO_NO_UPGRADE_CHECK", "true")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("{} --version: {error}", path.display()));
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert!(
            version.contains(&commit[..7]),
            "{} reports `{version}`, not the pinned commit {commit}; refusing an unpinned reference",
            path.display()
        );
        Some(Self {
            path,
            version,
            commit,
        })
    }
}

fn system_path() -> String {
    env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
    listener.local_addr().expect("local address").port()
}

/// One private Fabro server: its own home, storage, loopback port, dev token
/// and provider secrets, pointed at this case's twins.
pub(crate) struct FabroServer {
    pub(crate) binary:    FabroBinary,
    pub(crate) root:      PathBuf,
    pub(crate) url:       String,
    /// The fake credential this server hands the twins: its namespace.
    pub(crate) namespace: String,
    /// The redirected providers, by `lithos-llm` id.
    providers:            Vec<Provider>,
    child:                Child,
    pid:                  u32,
    http:                 reqwest::Client,
}

impl FabroServer {
    /// Start a server under `root` whose provider base URLs point at `twins`
    /// and whose provider secrets are `namespace`.
    ///
    /// # Panics
    ///
    /// Panics when the server does not become healthy within 60 seconds or
    /// the login and secret commands fail; the server log is in the message.
    pub(crate) async fn start(
        binary: FabroBinary,
        root: &Path,
        namespace: &str,
        twins: &[&Twin],
    ) -> Self {
        let home = root.join("home");
        let storage = root.join("storage");
        fs::create_dir_all(home.join(".fabro")).expect("fabro home");
        fs::create_dir_all(&storage).expect("fabro storage");
        let port = free_port().await;
        let url = format!("http://127.0.0.1:{port}");
        let mut settings = format!(
            "_version = 1\n\n[server.storage]\nroot = {storage:?}\n\n[server.auth]\nmethods = \
             [\"dev-token\"]\n\n[cli.target]\ntype = \"http\"\nurl = {url:?}\n\n[environments.\
             local]\nprovider = \"local\"\n",
            storage = storage.display().to_string(),
        );
        for twin in twins {
            // Fabro's adapters append `/responses` and `/messages` to the
            // provider base URL; the twins serve those under `/v1`.
            settings.push_str(&format!(
                "\n[llm.providers.{}]\nbase_url = \"{}/v1\"\n",
                twin.provider.id(),
                twin.base_url
            ));
        }
        let config = home.join(".fabro").join("settings.toml");
        fs::write(&config, settings).expect("write settings.toml");
        let log_path = root.join("server.log");
        let log = fs::File::create(&log_path).expect("server log");
        let err = log.try_clone().expect("server log handle");
        let providers: Vec<Provider> = twins.iter().map(|twin| twin.provider).collect();
        let mut command = Command::new(&binary.path);
        command
            .args([
                "server",
                "start",
                "--foreground",
                "--no-web",
                "--storage-dir",
            ])
            .arg(&storage)
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--config")
            .arg(&config)
            .env_clear()
            .envs(server_env(root, &url, namespace, &providers))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().expect("fabro server starts");
        let pid = child.id().expect("a running server has a pid");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("poll the server") {
                panic!(
                    "fabro server exited early with {status}:\n{}",
                    fs::read_to_string(&log_path).unwrap_or_default()
                );
            }
            let health = http
                .get(format!("{url}/api/v1/health"))
                .bearer_auth(DEV_TOKEN)
                .send()
                .await;
            if health.is_ok_and(|response| response.status().is_success()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fabro server did not become healthy:\n{}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
            sleep(Duration::from_millis(200)).await;
        }
        let server = Self {
            binary,
            root: root.to_path_buf(),
            url,
            namespace: namespace.to_owned(),
            providers,
            child,
            pid,
            http,
        };
        server
            .cli(&["auth", "login", "--dev-token", DEV_TOKEN], None)
            .await;
        // Run creation needs one ready provider even for a command-only
        // graph. The secret is the case namespace, so a request that reaches
        // a twin is attributable to this server.
        let secrets: Vec<&'static str> = if server.providers.is_empty() {
            vec![Provider::OpenAi.credential_env()]
        } else {
            server
                .providers
                .iter()
                .map(|provider| provider.credential_env())
                .collect()
        };
        for secret in secrets {
            server
                .cli(&["secret", "set", secret, &server.namespace], None)
                .await;
        }
        server
    }

    fn env(&self) -> Vec<(String, String)> {
        server_env(&self.root, &self.url, &self.namespace, &self.providers)
    }

    /// Run `fabro <args>` against this server and return its output. Fails
    /// the test on a non-zero exit.
    pub(crate) async fn cli(&self, args: &[&str], cwd: Option<&Path>) -> process::Output {
        let output = self.cli_unchecked(args, cwd).await;
        assert!(
            output.status.success(),
            "fabro {} failed ({}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Run `fabro <args>` and return its output whatever the exit code.
    pub(crate) async fn cli_unchecked(&self, args: &[&str], cwd: Option<&Path>) -> process::Output {
        let mut command = Command::new(&self.binary.path);
        command
            .args(args)
            .env_clear()
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        timeout(Duration::from_secs(120), command.output())
            .await
            .unwrap_or_else(|_| panic!("fabro {} hung", args.join(" ")))
            .unwrap_or_else(|error| panic!("fabro {}: {error}", args.join(" ")))
    }

    async fn api_get(&self, path: &str) -> Result<Value, String> {
        let response = self
            .http
            .get(format!("{}{path}", self.url))
            .bearer_auth(DEV_TOKEN)
            .send()
            .await
            .map_err(|error| format!("GET {path}: {error}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("GET {path}: {status}: {text}"));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|error| format!("GET {path}: {error}: {text}"))
    }

    async fn api_post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let response = self
            .http
            .post(format!("{}{path}", self.url))
            .bearer_auth(DEV_TOKEN)
            .json(body)
            .send()
            .await
            .map_err(|error| format!("POST {path}: {error}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("POST {path}: {status}: {text}"));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|error| format!("POST {path}: {error}: {text}"))
    }

    /// Validate, create and drive one run to completion, then collect its
    /// events, state, dump and interview receipt.
    pub(crate) async fn run(&self, launch: &FabroLaunch<'_>) -> FabroRun {
        let dump_dir = self.root.join("dump");
        let mut run = FabroRun {
            run_id:        None,
            validation:    json!({ "status": "accepted" }),
            launch_stdout: String::new(),
            launch_stderr: String::new(),
            status:        "not_started".to_owned(),
            timed_out:     false,
            events:        Vec::new(),
            state:         Value::Null,
            dump_dir:      dump_dir.clone(),
            receipt:       Value::Null,
            workspace:     launch.dir.to_path_buf(),
        };
        let validated = self
            .cli_unchecked(&["validate", "--json", launch.workflow], Some(launch.dir))
            .await;
        if !validated.status.success() {
            let stdout = String::from_utf8_lossy(&validated.stdout);
            let diagnostics = serde_json::from_str::<Value>(&stdout)
                .ok()
                .and_then(|report| report.get("diagnostics").cloned())
                .unwrap_or_else(|| {
                    json!(
                        String::from_utf8_lossy(&validated.stderr)
                            .trim()
                            .lines()
                            .collect::<Vec<_>>()
                    )
                });
            run.validation = json!({
                "status": "rejected",
                "rejected_by": "fabro validate",
                "diagnostics": diagnostics,
            });
            run.status = "rejected".to_owned();
            run.receipt = Interviews::load(launch.script).receipt();
            return run;
        }
        let mut args: Vec<String> = vec![
            "run".into(),
            "--detach".into(),
            "--json".into(),
            "--environment".into(),
            "local".into(),
            "--provider".into(),
            launch.provider.to_owned(),
        ];
        for (key, value) in launch.inputs {
            args.push("-I".into());
            args.push(format!("{key}={value}"));
        }
        args.push(launch.workflow.to_owned());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let created = self.cli_unchecked(&refs, Some(launch.dir)).await;
        run.launch_stdout = String::from_utf8_lossy(&created.stdout).into_owned();
        run.launch_stderr = String::from_utf8_lossy(&created.stderr).into_owned();
        assert!(
            created.status.success(),
            "fabro run failed:\n{}\n{}",
            run.launch_stdout,
            run.launch_stderr
        );
        let run_id = run_id_of(&run.launch_stdout)
            .unwrap_or_else(|| panic!("no run id in:\n{}", run.launch_stdout));
        run.run_id = Some(run_id.clone());

        let mut interviews = Interviews::load(launch.script);
        let deadline = Instant::now() + launch.deadline;
        loop {
            let state = self
                .api_get(&format!("/api/v1/runs/{run_id}/state"))
                .await
                .unwrap_or_else(|error| panic!("{error}"));
            let kind = status_kind(&state);
            if matches!(
                kind.as_str(),
                "succeeded" | "failed" | "cancelled" | "dead" | "completed"
            ) {
                run.status = kind;
                break;
            }
            let questions = self
                .api_get(&format!("/api/v1/runs/{run_id}/questions"))
                .await
                .unwrap_or_else(|error| panic!("{error}"));
            let items = questions
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| questions.as_array().cloned())
                .unwrap_or_default();
            for question in items {
                let qid = question["id"].as_str().unwrap_or_default().to_owned();
                if interviews.decided(&qid) {
                    continue;
                }
                let disposition = interviews.decide(&question);
                match disposition {
                    Disposition::Answer(body) => {
                        let result = self
                            .api_post(
                                &format!("/api/v1/runs/{run_id}/questions/{qid}/answer"),
                                &body,
                            )
                            .await;
                        interviews.delivered(&qid, result);
                    }
                    Disposition::Cancel => {
                        let _ = self
                            .api_post(&format!("/api/v1/runs/{run_id}/cancel"), &json!({}))
                            .await;
                    }
                    Disposition::Withhold | Disposition::Error => {}
                }
            }
            if Instant::now() >= deadline {
                run.timed_out = true;
                let _ = self
                    .api_post(&format!("/api/v1/runs/{run_id}/cancel"), &json!({}))
                    .await;
                run.status = "timed_out".to_owned();
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
        interviews.finish();
        run.receipt = interviews.receipt();

        let events = self.cli(&["events", "--json", &run_id], None).await;
        run.events = String::from_utf8_lossy(&events.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        fs::write(self.root.join("events.jsonl"), &events.stdout).expect("write events.jsonl");
        run.state = self
            .api_get(&format!("/api/v1/runs/{run_id}/state"))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        fs::write(
            self.root.join("state.json"),
            serde_json::to_vec_pretty(&run.state).expect("state"),
        )
        .expect("write state.json");
        let _ = fs::remove_dir_all(&dump_dir);
        let dumped = self
            .cli_unchecked(
                &["dump", "--output", &dump_dir.to_string_lossy(), &run_id],
                None,
            )
            .await;
        if !dumped.status.success() {
            eprintln!(
                "fabro dump failed for {run_id}: {}",
                String::from_utf8_lossy(&dumped.stderr)
            );
        }
        if run.status == "completed" {
            run.status = terminal_status(&run.events).unwrap_or(run.status);
        }
        run
    }

    /// Stop the server and reap its process group.
    pub(crate) async fn stop(mut self) {
        let _ = self.child.start_kill();
        kill_group(self.pid);
        let _ = timeout(Duration::from_secs(20), self.child.wait()).await;
    }
}

impl Drop for FabroServer {
    fn drop(&mut self) {
        kill_group(self.pid);
    }
}

fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = pid;
}

fn server_env(
    root: &Path,
    url: &str,
    namespace: &str,
    providers: &[Provider],
) -> Vec<(String, String)> {
    let home = root.join("home");
    let mut env = vec![
        ("PATH".to_owned(), system_path()),
        ("HOME".to_owned(), home.to_string_lossy().into_owned()),
        (
            "FABRO_HOME".to_owned(),
            home.join(".fabro").to_string_lossy().into_owned(),
        ),
        ("FABRO_SERVER".to_owned(), url.to_owned()),
        ("FABRO_DEV_TOKEN".to_owned(), DEV_TOKEN.to_owned()),
        ("SESSION_SECRET".to_owned(), SESSION_SECRET.to_owned()),
        ("FABRO_NO_UPGRADE_CHECK".to_owned(), "true".to_owned()),
        ("FABRO_HTTP_PROXY_POLICY".to_owned(), "disabled".to_owned()),
        ("FABRO_TELEMETRY".to_owned(), "off".to_owned()),
        ("FABRO_SUPPRESS_OPEN_BROWSER".to_owned(), "1".to_owned()),
        ("FABRO_TEST_IN_MEMORY_STORE".to_owned(), "1".to_owned()),
        ("NO_COLOR".to_owned(), "1".to_owned()),
        // The system temp dir, not a per-case one: Fabro binds Unix sockets
        // under it and a long path exceeds the socket path limit.
        (
            "TMPDIR".to_owned(),
            env::temp_dir().to_string_lossy().into_owned(),
        ),
    ];
    for provider in providers {
        env.push((provider.credential_env().to_owned(), namespace.to_owned()));
    }
    env
}

/// The run id `fabro run --detach --json` printed.
fn run_id_of(stdout: &str) -> Option<String> {
    if let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(stdout) {
        if let Some(id) = doc.get("run_id").and_then(Value::as_str) {
            return Some(id.to_owned());
        }
    }
    for line in stdout.lines() {
        if let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(line.trim()) {
            if let Some(id) = doc.get("run_id").and_then(Value::as_str) {
                return Some(id.to_owned());
            }
        }
        if let Some(rest) = line.trim().strip_prefix("Run: ") {
            return rest.split_whitespace().next().map(str::to_owned);
        }
    }
    None
}

/// The status word of a run state document: `status` is a string or an
/// object with `kind`.
fn status_kind(state: &Value) -> String {
    match &state["status"] {
        Value::String(kind) => kind.clone(),
        Value::Object(map) => map
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// The terminal status the event log records.
pub(crate) fn terminal_status(events: &[Value]) -> Option<String> {
    let mut status = None;
    for event in events {
        match event["event"].as_str() {
            Some("run.completed") => status = Some("succeeded".to_owned()),
            Some("run.failed") => status = Some("failed".to_owned()),
            Some("run.cancelled") => status = Some("cancelled".to_owned()),
            _ => {}
        }
    }
    status
}

/// What one Fabro run needs: the staged bundle directory (a git repository
/// Fabro's `local` environment works in), the workflow file inside it, the
/// concrete inputs, the default provider, and the shared interview script.
pub(crate) struct FabroLaunch<'a> {
    pub(crate) dir:      &'a Path,
    pub(crate) workflow: &'a str,
    pub(crate) inputs:   &'a [(&'a str, String)],
    pub(crate) provider: &'a str,
    /// The same `cli::answer` script the Petri side runs with. `None`
    /// means no interviewer: any question is an error.
    pub(crate) script:   Option<&'a Path>,
    pub(crate) deadline: Duration,
}

/// What one Fabro run left behind.
pub(crate) struct FabroRun {
    pub(crate) run_id:        Option<String>,
    pub(crate) validation:    Value,
    pub(crate) launch_stdout: String,
    pub(crate) launch_stderr: String,
    /// `succeeded`, `failed`, `cancelled`, `dead`, `rejected` (by the
    /// validator) or `timed_out` (cancelled by the adapter's deadline).
    pub(crate) status:        String,
    pub(crate) timed_out:     bool,
    pub(crate) events:        Vec<Value>,
    pub(crate) state:         Value,
    pub(crate) dump_dir:      PathBuf,
    /// The interview receipt in the same shape Petri writes
    /// (`interviews.json`): `questions`, `errors`, `script.entries`.
    pub(crate) receipt:       Value,
    /// The working directory the run used: the staged bundle itself.
    pub(crate) workspace:     PathBuf,
}

impl FabroRun {
    /// Events of one kind, in order.
    pub(crate) fn events_of(&self, kind: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event["event"] == kind)
            .collect()
    }

    /// The `parallel_results.json` files the dump wrote, keyed by the stage
    /// directory name (`<rank>-<node>@<visit>`), in rank order. These carry
    /// the inline values the event log offloads to `blob://` references.
    pub(crate) fn dumped_parallel_results(&self) -> Vec<(String, Value)> {
        let stages = self.dump_dir.join("stages");
        let Ok(entries) = fs::read_dir(&stages) else {
            return Vec::new();
        };
        let mut found: Vec<(String, Value)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path().join("parallel_results.json");
                let text = fs::read_to_string(&path).ok()?;
                let value = serde_json::from_str(&text).ok()?;
                Some((entry.file_name().to_string_lossy().into_owned(), value))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }
}

/// How the adapter disposes of one pending question.
enum Disposition {
    Answer(Value),
    Cancel,
    Withhold,
    Error,
}

/// One entry of the shared interview script, with its consumption state.
struct Entry {
    id:        String,
    node:      Option<String>,
    kind:      Option<String>,
    text:      Option<String>,
    contains:  Option<String>,
    options:   Option<Vec<String>>,
    count:     u64,
    consumed:  u64,
    required:  bool,
    action:    Value,
    questions: Vec<String>,
}

impl Entry {
    fn matches(&self, question: &Value) -> bool {
        let stage = question["stage"].as_str().unwrap_or_default();
        let kind = question_kind(question);
        let text = question["text"].as_str().unwrap_or_default();
        let keys = option_keys(question);
        self.node.as_deref().is_none_or(|node| node == stage)
            && self.kind.as_deref().is_none_or(|k| k == kind)
            && self.text.as_deref().is_none_or(|t| t == text)
            && self.contains.as_deref().is_none_or(|t| text.contains(t))
            && self.options.as_ref().is_none_or(|options| *options == keys)
    }
}

/// The question kind in Petri's vocabulary (`yes_no`, `confirmation`,
/// `multiple_choice`, `multi_select`, `freeform`), from Fabro's
/// `question_type`.
fn question_kind(question: &Value) -> String {
    let raw = match &question["question_type"] {
        Value::String(kind) => kind.clone(),
        other => other.to_string().trim_matches('"').to_owned(),
    };
    let mut out = String::new();
    for (index, ch) in raw.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The option keys as Petri's interviewer sees them: the accelerator of a
/// `[K] Label` label, else the option's own key.
fn option_keys(question: &Value) -> Vec<String> {
    question["options"]
        .as_array()
        .map(|options| options.iter().map(option_key).collect())
        .unwrap_or_default()
}

fn option_key(option: &Value) -> String {
    let label = option["label"].as_str().unwrap_or_default();
    if let Some(rest) = label.strip_prefix('[') {
        if let Some((key, _)) = rest.split_once(']') {
            return key.trim().to_owned();
        }
    }
    option["key"].as_str().unwrap_or_default().to_owned()
}

/// Find the Fabro option a scripted value names: by Petri key, by Fabro key,
/// or by label, case-insensitively.
fn find_option<'a>(question: &'a Value, value: &str) -> Option<&'a Value> {
    let wanted = value.trim().to_lowercase();
    question["options"].as_array()?.iter().find(|option| {
        let key = option["key"].as_str().unwrap_or_default().to_lowercase();
        let label = option["label"].as_str().unwrap_or_default().to_lowercase();
        let petri = option_key(option).to_lowercase();
        petri == wanted
            || key == wanted
            || label == wanted
            || label.strip_prefix('[').is_some_and(|rest| {
                rest.split_once(']')
                    .is_some_and(|(_, text)| text.trim() == wanted)
            })
    })
}

/// Whether a matched option is the affirmative one of a yes/no gate.
fn is_affirmative(option: &Value) -> bool {
    let key = option["key"].as_str().unwrap_or_default().to_lowercase();
    let label = option["label"].as_str().unwrap_or_default().to_lowercase();
    let text = label
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map_or(label.as_str(), |(_, text)| text.trim())
        .to_owned();
    key == "yes" || key == "y" || text.starts_with('y')
}

/// The shared interview script driven through Fabro's public answer
/// mechanism, and the receipt it produces.
struct Interviews {
    entries:   Vec<Entry>,
    decided:   BTreeMap<String, usize>,
    questions: Vec<Value>,
    errors:    Vec<String>,
}

impl Interviews {
    fn load(path: Option<&Path>) -> Self {
        let entries = path
            .map(|path| {
                let text = fs::read_to_string(path).expect("read the interview script");
                let script: Value = serde_json::from_str(&text).expect("the script is JSON");
                script["entries"]
                    .as_array()
                    .expect("entries")
                    .iter()
                    .map(|entry| {
                        let matcher = &entry["match"];
                        let strings = |key: &str| matcher[key].as_str().map(str::to_owned);
                        Entry {
                            id:        entry["id"].as_str().unwrap_or_default().to_owned(),
                            node:      strings("node"),
                            kind:      strings("kind"),
                            text:      strings("text"),
                            contains:  strings("text_contains"),
                            options:   matcher["options"].as_array().map(|options| {
                                options
                                    .iter()
                                    .map(|o| o.as_str().unwrap_or_default().to_owned())
                                    .collect()
                            }),
                            count:     entry["count"].as_u64().unwrap_or(1),
                            consumed:  0,
                            required:  entry["required"].as_bool().unwrap_or(true),
                            action:    entry["action"].clone(),
                            questions: Vec::new(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            entries,
            decided: BTreeMap::new(),
            questions: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn decided(&self, qid: &str) -> bool {
        self.decided.contains_key(qid)
    }

    fn decide(&mut self, question: &Value) -> Disposition {
        let qid = question["id"].as_str().unwrap_or_default().to_owned();
        let stage = question["stage"].as_str().unwrap_or_default().to_owned();
        let kind = question_kind(question);
        let text = question["text"].as_str().unwrap_or_default().to_owned();
        let keys = option_keys(question);
        let candidates: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.consumed < entry.count && entry.matches(question))
            .map(|(index, _)| index)
            .collect();
        let mut record = json!({
            "node": stage,
            "kind": kind,
            "text": text,
            "options": keys,
            "question": qid,
            "delivery": "withheld",
        });
        let index = match candidates.as_slice() {
            [] => {
                self.errors.push(format!(
                    "no script entry matches question {qid} on `{stage}` ({kind}: {text})"
                ));
                record["delivery"] = json!("unmatched");
                self.decided.insert(qid.clone(), usize::MAX);
                self.questions.push(record);
                return Disposition::Error;
            }
            [one] => *one,
            many => {
                let ids: Vec<&str> = many.iter().map(|i| self.entries[*i].id.as_str()).collect();
                self.errors.push(format!(
                    "question {qid} on `{stage}` matches more than one entry: {ids:?}"
                ));
                record["delivery"] = json!("ambiguous");
                self.decided.insert(qid.clone(), usize::MAX);
                self.questions.push(record);
                return Disposition::Error;
            }
        };
        let entry = &mut self.entries[index];
        entry.consumed += 1;
        entry.questions.push(qid.clone());
        record["entry"] = json!(entry.id);
        self.decided.insert(qid.clone(), self.questions.len());
        let action = entry.action.clone();
        let action_kind = action["kind"].as_str().unwrap_or_default();
        let value = action["value"].as_str().unwrap_or_default().to_owned();
        let disposition = match action_kind {
            "choice" => match find_option(question, &value) {
                Some(option) => {
                    record["reply"] = json!({ "kind": "answered", "choice": value });
                    let body = if matches!(kind.as_str(), "yes_no" | "confirmation") {
                        json!({ "kind": if is_affirmative(option) { "yes" } else { "no" } })
                    } else {
                        json!({ "kind": "selected", "option_key": option["key"] })
                    };
                    Disposition::Answer(body)
                }
                None => {
                    self.errors.push(format!(
                        "entry `{}` chooses `{value}`, which question {qid} on `{stage}` does not offer ({keys:?})",
                        entry.id
                    ));
                    record["delivery"] = json!("invalid");
                    Disposition::Error
                }
            },
            "choices" => {
                let values: Vec<String> = action["values"]
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .map(|x| x.as_str().unwrap_or_default().to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                let mapped: Option<Vec<Value>> = values
                    .iter()
                    .map(|v| find_option(question, v).map(|o| o["key"].clone()))
                    .collect();
                match mapped {
                    Some(option_keys) => {
                        record["reply"] = json!({ "kind": "answered", "choices": values });
                        Disposition::Answer(
                            json!({ "kind": "multi_selected", "option_keys": option_keys }),
                        )
                    }
                    None => {
                        self.errors.push(format!(
                            "entry `{}` selects {values:?}, not all offered by question {qid}",
                            entry.id
                        ));
                        record["delivery"] = json!("invalid");
                        Disposition::Error
                    }
                }
            }
            "text" => {
                record["reply"] = json!({ "kind": "answered", "text": value });
                Disposition::Answer(json!({ "kind": "text", "text": value }))
            }
            "negative" => {
                record["reply"] = json!({ "kind": "negative" });
                if matches!(kind.as_str(), "yes_no" | "confirmation") {
                    Disposition::Answer(json!({ "kind": "no" }))
                } else {
                    self.errors.push(format!(
                        "entry `{}` answers negatively, but question {qid} ({kind}) has no negative answer in Fabro",
                        entry.id
                    ));
                    Disposition::Error
                }
            }
            "invalid" => {
                record["reply"] = json!({ "kind": "invalid", "value": value });
                Disposition::Answer(json!({ "kind": "selected", "option_key": value }))
            }
            "cancel" => {
                record["reply"] = json!({ "kind": "cancelled" });
                record["delivery"] = json!("cancelled");
                Disposition::Cancel
            }
            "withhold" => {
                record["reply"] = json!({ "kind": "withheld" });
                Disposition::Withhold
            }
            other => {
                self.errors.push(format!(
                    "entry `{}` has an action `{other}` the adapter does not know",
                    entry.id
                ));
                Disposition::Error
            }
        };
        self.questions.push(record);
        disposition
    }

    fn delivered(&mut self, qid: &str, result: Result<Value, String>) {
        let Some(index) = self.decided.get(qid).copied() else {
            return;
        };
        let Some(record) = self.questions.get_mut(index.wrapping_sub(1)) else {
            return;
        };
        match result {
            Ok(_) => record["delivery"] = json!("delivered"),
            Err(error) => {
                let invalid = record["reply"]["kind"] == "invalid";
                record["delivery"] = json!("rejected");
                record["rejection"] = json!(error);
                if !invalid {
                    self.errors
                        .push(format!("answer to question {qid} was rejected: {error}"));
                }
            }
        }
    }

    fn finish(&mut self) {
        for entry in &self.entries {
            if entry.required && entry.consumed < entry.count {
                self.errors.push(format!(
                    "required entry `{}` was used {} of {} times",
                    entry.id, entry.consumed, entry.count
                ));
            }
        }
    }

    fn receipt(&self) -> Value {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "count": entry.count,
                    "consumed": entry.consumed,
                    "remaining": entry.count - entry.consumed,
                    "required": entry.required,
                    "questions": entry.questions,
                })
            })
            .collect();
        let mut receipt = Map::new();
        receipt.insert("version".into(), json!(1));
        receipt.insert("engine".into(), json!("fabro"));
        receipt.insert("questions".into(), json!(self.questions));
        receipt.insert("errors".into(), json!(self.errors));
        receipt.insert("script".into(), json!({ "entries": entries }));
        Value::Object(receipt)
    }
}

/// Copy a scenario bundle into `dest` and make it a git repository with one
/// commit, which is what Fabro's `local` environment and its helpers
/// (`review_head()`) expect to find. Petri's side stages the same bundle and
/// starts its workspace empty; both sides then run the bundle's own setup.
pub(crate) fn stage_repository(source: &Path, dest: &Path) {
    copy_tree(source, dest).unwrap_or_else(|error| {
        panic!(
            "could not stage {} into {}: {error}",
            source.display(),
            dest.display()
        )
    });
    let git = |args: &[&str]| {
        let status = process::Command::new("git")
            .args(args)
            .current_dir(dest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .expect("git runs");
        assert!(
            status.success(),
            "git {} failed in {}",
            args.join(" "),
            dest.display()
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=petri-differential",
        "-c",
        "user.email=petri-differential@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "staged bundle",
    ]);
}

pub(crate) fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
