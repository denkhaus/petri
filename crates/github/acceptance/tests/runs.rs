//! The corpus run sweep: every in-scope workflow that lowers, run end to end,
//! `run:` scripts stubbed to `true` (builds are out of budget and off the
//! metric), `uses:` steps real over the corpus's pinned source trees, every
//! host scope rewritten to a pinned runner container so corpus code never
//! executes on the host. Writes `crates/github/corpus/RUNS.md` — the run-time
//! REPORT.md.
//!
//! Opt-in like the snapshot refresh: it needs the network (action fetches, real
//! checkouts) and a Docker daemon, and a full pass runs hundreds of containers.
//!
//! ```text
//! cargo test -p petri-github-acceptance --test runs -- --ignored --nocapture
//! ```
//!
//! Scale controls, so the sweep stays runnable after every commit:
//! `PETRI_SWEEP_JOBS` bounds workflow parallelism (default 4),
//! `PETRI_SWEEP_TIMEOUT` caps each workflow's wall clock in seconds (default
//! 300) so one wedged workflow cannot stall the battery, and
//! `PETRI_SWEEP_FILTER` narrows the sweep to workflows whose `repo/file`
//! contains the substring — the dev loop for a single repository.
//!
//! The sweep is token-less by default (see [`sweep_token`]); `PETRI_SWEEP_TOKEN`
//! opts a real token in — `PETRI_SWEEP_TOKEN=$(gh auth token)` — for a
//! measurement free of GitHub's anonymous rate limit. Opt-in only: corpus code
//! then runs with that identity, bounded by the token's own permissions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use acceptance::runs::{
    self, FirstFailure, RunRecord, RunResult, StepIdentity, battery_image, expected_from_tail,
    expected_reason, identity_of, runs_report, step_identities,
};
use acceptance::{Class, corpus_present, lower_one, workflows};
use github_actions::{ActionSource, ActionSourceCap, ActionTreeSource, GitActionSource};
use runtime::driver::RunReport;
use runtime::executor::{MapSecrets, Retention};
use runtime::ir::{self, Graph};
use runtime::{RunOptions, Runtime};
use serde_json::json;

/// The sweep's `GITHUB_TOKEN`: `PETRI_SWEEP_TOKEN` when the operator opted a
/// real one in, else **empty**. Empty still resolves `${{ secrets.GITHUB_TOKEN }}`
/// and `github.token`, and the toolkit treats it as "no auth" — API calls go
/// anonymous (the setup-* version manifests, github-script reads) and succeed
/// where a *bogus* value gets `401 Bad credentials`: GitHub accepts absent
/// credentials and rejects invalid ones. Token-less by default, so corpus code
/// can never act with the user's identity; it is also the value a bare machine
/// with no `gh` login gets from the distribution. The anonymous tier is
/// rate-limited (60/hour/IP), which a whole-corpus sweep exceeds — hence the
/// opt-in.
fn sweep_token() -> String {
    std::env::var("PETRI_SWEEP_TOKEN").unwrap_or_default()
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "network + docker: runs the corpus end to end and rewrites crates/github/corpus/RUNS.md"]
async fn corpus_run_sweep() {
    let root = corpus_root();
    if !corpus_present(&root) {
        eprintln!("skipping: corpus not fetched (scripts/corpus-fetch.sh)");
        return;
    }
    if !testkit::docker_ready().await {
        return;
    }

    let source = Arc::new(GitActionSource::new(root.join(".actions-cache")));
    let manifests: Arc<dyn ActionSource> = source.clone();
    let trees: Arc<dyn ActionTreeSource> = source;

    let filter = std::env::var("PETRI_SWEEP_FILTER").unwrap_or_default();
    let jobs = env_num("PETRI_SWEEP_JOBS", 4) as usize;
    let timeout = Duration::from_secs(env_num("PETRI_SWEEP_TIMEOUT", 300));

    let pins = corpus_pins(&root);

    // Lower everything serially first: it fills the action caches while the
    // classification decides what runs. Excluded files (Windows/macOS,
    // callee-only, broken upstream) leave the denominator exactly as REPORT.md
    // leaves them.
    let mut records: Vec<RunRecord> = Vec::new();
    let mut queue: Vec<(usize, Graph, bool)> = Vec::new();
    for (repo, repo_root, file) in workflows(&root) {
        let (outcome, graph) = lower_one(&repo, &repo_root, &file, Some(&manifests));
        if outcome.out_of_scope() || outcome.broken_upstream() || outcome.callee_only() {
            continue;
        }
        if !filter.is_empty() && !format!("{repo}/{}", outcome.file).contains(&filter) {
            continue;
        }
        let record = RunRecord {
            repo: repo.clone(),
            file: outcome.file.clone(),
            result: RunResult::NotLowered {
                features: outcome.unsupported_features(),
            },
        };
        if let (Class::Clean | Class::Warnings, Some(mut graph)) = (outcome.class, graph) {
            prepare(&mut graph, &repo, pins.get(&repo), &repo_root);
            // A reusable file run standalone has no caller to supply its
            // declared inputs; a firing-environment failure there is
            // caller-coupled, not a gap. (The word in the file is the signal:
            // hybrid-trigger files whose env broke on absent inputs would
            // break on GitHub's own non-call triggers too.)
            let caller_coupled =
                std::fs::read_to_string(&file).is_ok_and(|text| text.contains("workflow_call"));
            queue.push((records.len(), graph, caller_coupled));
        }
        records.push(record);
    }
    let to_run = queue.len();
    eprintln!(
        "sweep: {} in scope, {to_run} to run, {} not lowered",
        records.len(),
        records.len() - to_run
    );

    let semaphore = Arc::new(tokio::sync::Semaphore::new(jobs));
    let mut set = tokio::task::JoinSet::new();
    for (slot, graph, caller_coupled) in queue {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore open");
        let trees = Arc::clone(&trees);
        let repo = records[slot].repo.clone();
        let file = records[slot].file.clone();
        set.spawn(async move {
            let result = run_one(&repo, &file, graph, trees, timeout, caller_coupled).await;
            drop(permit);
            (slot, result)
        });
    }
    let mut done = 0usize;
    while let Some(joined) = set.join_next().await {
        let (slot, result) = joined.expect("a sweep task never panics");
        done += 1;
        eprintln!(
            "  [{done}/{to_run}] {} {} — {}",
            records[slot].repo,
            records[slot].file.trim_start_matches(".github/workflows/"),
            match &result {
                RunResult::Pass => "pass".to_string(),
                RunResult::Fail(f) => format!(
                    "fail at `{}` ({}){}",
                    f.step,
                    f.class,
                    if f.expected.is_some() {
                        " [expected]"
                    } else {
                        ""
                    }
                ),
                RunResult::TimedOut { wedged } =>
                    format!("timeout{}", if *wedged { " (wedged)" } else { "" }),
                RunResult::NotLowered { .. } => "not lowered".to_string(),
            }
        );
        records[slot].result = result;
    }

    let auth = if sweep_token().is_empty() {
        "an empty `GITHUB_TOKEN` kept the sweep token-less (actions' API calls went \
         anonymous, as on a machine with no `gh` login — rate-limited at 60/hour)"
    } else {
        "a real `GITHUB_TOKEN` (`PETRI_SWEEP_TOKEN`) authenticated actions' API \
         calls, so no anonymous rate limit applied"
    };
    let note = format!(
        "Sweep configuration: host scopes rewritten to the pinned runner images \
         `{}` (22.04/26.04 variants by label), matrices capped to their first leg, \
         each workflow capped at {}s wall clock, parallelism {jobs}. Identity: \
         `github.sha` is the repo's pinned corpus commit (`corpus-pins.txt`) — the \
         sweep's analog of `default_params` reading HEAD — so `checkout` fetches \
         real state; {auth}.",
        runs::RUNNER_IMAGE_2404,
        timeout.as_secs(),
    );
    let markdown = runs_report(&records, &note);
    let path = root.join("RUNS.md");
    std::fs::write(&path, &markdown).expect("write the runs report");
    eprintln!("wrote {}", path.display());
}

/// Sweep shape: scripts stubbed, host scopes containerized, matrices capped,
/// and the run parameters a host would fill — the corpus directory's slug as
/// the repository and its **pinned commit** as `github.sha`, the sweep's
/// analog of `default_params` reading the checkout's HEAD. A corpus dir is
/// workflows without a checkout, so the honest identity comes from
/// `corpus-pins.txt` and the fetch's recorded default branch; `checkout` then
/// fetches a commit that exists.
fn prepare(graph: &mut Graph, repo_slug: &str, pin: Option<&String>, repo_root: &Path) {
    runs::stub_run_scripts(graph);
    // A graph that drives a Docker engine gets the dind runner (and the
    // `--privileged` its daemon needs) where the 24.04 image would have been
    // picked — the only flavor the dind variant is built for.
    let docker = runs::needs_docker(graph);
    runs::containerize(graph, |requirements| {
        let image = battery_image(requirements);
        if docker && image == runs::RUNNER_IMAGE_2404 {
            runs::RUNNER_IMAGE_2404_DIND.to_string()
        } else {
            image.to_string()
        }
    });
    if docker {
        runs::privilege(graph, runs::RUNNER_IMAGE_2404_DIND);
    }
    runs::cap_expansions(graph);
    let mut github = frontend_gha::identity::github_context(Some(repo_slug));
    github["event"] = json!({});
    if let Some(sha) = pin {
        let branch = default_branch(repo_root).unwrap_or_else(|| "main".to_string());
        github["sha"] = json!(sha);
        github["ref"] = json!(format!("refs/heads/{branch}"));
        github["ref_name"] = json!(branch);
    }
    graph.params.insert("github".into(), github);
    graph.params.insert(
        "runner".into(),
        json!({"os": "Linux", "arch": "X64", "name": "petri-sweep"}),
    );
    graph.params.insert("vars".into(), json!({}));
    // The corpus dir is the "repository" the substituted checkout materializes:
    // a plain workflow tree, no `.git` — copied wholesale, offline.
    graph.params.insert(
        "petri".into(),
        json!({ "repo": repo_root.display().to_string() }),
    );
}

/// `corpus-pins.txt`: `owner/repo <sha>` per line — the commit each corpus
/// repo was fetched at, which is the commit its workflows describe.
fn corpus_pins(corpus_root: &Path) -> std::collections::BTreeMap<String, String> {
    let text = std::fs::read_to_string(corpus_root.join("../corpus-pins.txt")).unwrap_or_default();
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (repo, sha) = line.split_once(' ')?;
            let sha = sha.trim();
            (sha.len() == 40).then(|| (repo.to_string(), sha.to_string()))
        })
        .collect()
}

/// The default branch the fetch recorded in the repo's PROVENANCE.md.
fn default_branch(repo_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo_root.join("PROVENANCE.md")).ok()?;
    let line = text.lines().find(|l| l.contains("default branch:"))?;
    let (_, rest) = line.split_once("default branch:")?;
    Some(rest.trim().trim_end_matches(')').to_string())
}

async fn run_one(
    repo: &str,
    file: &str,
    graph: Graph,
    trees: Arc<dyn ActionTreeSource>,
    timeout: Duration,
    caller_coupled: bool,
) -> RunResult {
    let identities = step_identities(&graph);
    let label: String = format!("{repo}-{file}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = std::env::temp_dir()
        .join("petri-sweep")
        .join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut options = RunOptions::new(&dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    // The sweep measures outcomes, not determinism; replay verification is the
    // acceptance battery's business.
    options.verify_replay = false;
    let rt = Runtime::standard()
        .options(options)
        .step(github_actions::RunStep)
        .step(github_actions::ActionStep)
        .step(github_actions::DockerActionStep)
        .step(github_actions::CheckoutStep)
        .capability(ActionSourceCap(trees))
        .secrets(MapSecrets::from_pairs(&[("GITHUB_TOKEN", &sweep_token())]));

    let driver = rt.driver(graph);
    let handle = driver.handle();
    let mut run = tokio::spawn(driver.run());

    let outcome = tokio::select! {
        joined = &mut run => Finished::Ran(Box::new(joined.expect("the driver task never panics"))),
        _ = tokio::time::sleep(timeout) => {
            handle.cancel(ir::CancelScopeId::ROOT).await;
            match tokio::time::timeout(Duration::from_secs(90), &mut run).await {
                Ok(joined) => {
                    let _ = joined.expect("the driver task never panics");
                    Finished::TimedOut
                }
                Err(_) => {
                    // The cancel did not bring the run down. Abandon the task and
                    // remove whatever containers the run dir's id names, since no
                    // release will.
                    run.abort();
                    sweep_leftovers(&dir).await;
                    Finished::Wedged
                }
            }
        }
    };

    let result = match outcome {
        Finished::Wedged => RunResult::TimedOut { wedged: true },
        Finished::TimedOut => RunResult::TimedOut { wedged: false },
        Finished::Ran(report) => match report.status {
            ir::RunStatus::Success => RunResult::Pass,
            _ => first_failure(&report, &identities, caller_coupled),
        },
    };
    let _ = std::fs::remove_dir_all(&dir);
    result
}

enum Finished {
    Ran(Box<RunReport>),
    TimedOut,
    Wedged,
}

/// The run's first failing record, read against the graph's step identities.
fn first_failure(
    report: &RunReport,
    identities: &std::collections::BTreeMap<String, StepIdentity>,
    caller_coupled: bool,
) -> RunResult {
    for record in report.state.history() {
        if !record.outcome.status.is_failure() {
            continue;
        }
        let (class, message) = match record.outcome.status.failure_info() {
            Some(info) => (info.class.to_string(), info.message.clone()),
            None => (record.outcome.status.tag().to_string(), String::new()),
        };
        let identity = identity_of(identities, &record.name)
            .cloned()
            .unwrap_or_else(|| StepIdentity::Other(record.name.to_string()));
        let tail = log_tail(report, record.firing);
        let expected = expected_reason(&identity, &class, !sweep_token().is_empty())
            .or_else(|| expected_from_tail(&identity, &tail))
            .or_else(|| {
                (caller_coupled && class == runtime::engine::FIRING_ENV_CLASS).then(|| {
                    "requires its caller's inputs (a reusable workflow run standalone)".to_string()
                })
            });
        return RunResult::Fail(FirstFailure {
            node: record.name.to_string(),
            step: identity.label(),
            class,
            message,
            tail,
            expected,
        });
    }
    // No failing record: the run failed on engine errors alone — name them,
    // or the report can only shrug.
    let errors: Vec<String> = report
        .state
        .errors()
        .iter()
        .map(|e| format!("{e}"))
        .collect();
    RunResult::Fail(FirstFailure {
        node: "(run)".to_string(),
        step: "(run)".to_string(),
        class: String::new(),
        message: if errors.is_empty() {
            format!("run ended {:?} with no failing record", report.status)
        } else {
            format!("engine error: {}", errors.join(" | "))
        },
        tail: Vec::new(),
        expected: None,
    })
}

/// The failing firing's last log lines — what the failure actually said. With
/// `PETRI_SWEEP_LOG` set, the whole failing log goes to stderr — the dev loop
/// for one workflow's failure.
fn log_tail(report: &RunReport, firing: ir::FiringId) -> Vec<String> {
    let lines: Vec<String> = report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            runtime::engine::Event::StepProgress {
                firing: f,
                ev: ir::StepEvent::Log { line, .. },
            } if *f == firing => Some(line.clone()),
            _ => None,
        })
        .collect();
    if std::env::var("PETRI_SWEEP_LOG").is_ok_and(|v| !v.is_empty()) {
        for line in &lines {
            eprintln!("    | {line}");
        }
    }
    lines.into_iter().rev().take(4).rev().collect()
}

/// Remove the containers a wedged, abandoned run left behind: everything under
/// the run dir's recorded container-name prefix.
async fn sweep_leftovers(dir: &Path) {
    let Ok(id) = std::fs::read_to_string(dir.join(runtime::executor::docker::RUN_ID_FILE)) else {
        return;
    };
    let prefix = format!("petri-{}-", id.trim());
    let Ok(listed) = tokio::process::Command::new("docker")
        .args(["ps", "-a", "--format", "{{.Names}}"])
        .output()
        .await
    else {
        return;
    };
    for name in String::from_utf8_lossy(&listed.stdout).lines() {
        if name.starts_with(&prefix) {
            let _ = tokio::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output()
                .await;
        }
    }
}
