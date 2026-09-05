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
//! 900 — real toolchain installs and image builds outlive a tighter cap) so
//! one wedged workflow cannot stall the battery, `PETRI_SWEEP_FILTER` narrows
//! the sweep to workflows whose `repo/file` contains the substring — the dev
//! loop for a single repository — and `PETRI_SWEEP_PLATFORM` (e.g.
//! `linux/amd64`) forces the runner containers' platform: unset, the daemon
//! runs its native architecture and `runner.arch` says so; set to amd64 on an
//! arm64 host, the sweep reproduces GitHub's x64 runners under emulation.
//! Each workflow fires as its own primary trigger with a simulated event
//! payload — fields real (identity from the pin), resources fabricated
//! (#999999 exists nowhere server-side, so nothing real can be read or
//! mutated) — which is what lets `github.event.*` reads and `event_name`
//! conditions behave as they do on GitHub; `PETRI_SWEEP_EVENT=dispatch`
//! restores the bare dispatch + empty event.
//!
//! The sweep is token-less by default (see [`sweep_token`]);
//! `PETRI_SWEEP_TOKEN` opts a real token in — `PETRI_SWEEP_TOKEN=$(gh auth
//! token)` — for a measurement free of GitHub's anonymous rate limit. Opt-in
//! only: corpus code then runs with that identity, bounded by the token's own
//! permissions.

use std::collections::{BTreeMap, BTreeSet};
use std::env::{self, consts};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, process};

mod support;

use acceptance::runs::{
    self, FirstFailure, RunRecord, RunResult, StepIdentity, battery_image, expected_from_log,
    expected_reason, identity_of, runs_report, step_identities,
};
use acceptance::{Artifact, Class, has_corpus, lower_one, workflows};
use execution::host::{self, HostRun};
use execution::{
    CoordinatorEvent, CoordinatorRecord, ExecutionId, ExecutionObserver, InvocationId,
};
use frontend_gha::identity;
use github_actions::{ActionManifestSourceCap, ActionSource, ActionSourceCap, GitActionSource};
use runtime::driver::ExecutionReport;
use runtime::engine::{EngineState, Event, EventRecord, FIRING_ENV_CLASS};
use runtime::executor::sandbox::RUN_ID_FILE;
use runtime::executor::{MapSecrets, Retention};
use runtime::ir::{self, Graph};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinSet;
use tokio::time;

/// The sweep's `GITHUB_TOKEN`: `PETRI_SWEEP_TOKEN` when the operator opted a
/// real one in, else **empty**. Empty still resolves `${{ secrets.GITHUB_TOKEN
/// }}` and `github.token`, and the toolkit treats it as "no auth" — API calls
/// go anonymous (the setup-* version manifests, github-script reads) and
/// succeed where a *bogus* value gets `401 Bad credentials`: GitHub accepts
/// absent credentials and rejects invalid ones. Token-less by default, so
/// corpus code can never act with the user's identity; it is also the value a
/// bare machine with no `gh` login gets from the distribution. The anonymous
/// tier is rate-limited (60/hour/IP), which a whole-corpus sweep exceeds —
/// hence the opt-in.
fn sweep_token() -> String {
    env::var("PETRI_SWEEP_TOKEN").unwrap_or_default()
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

fn env_num(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "network + docker: runs the corpus end to end and rewrites crates/github/corpus/RUNS.md"]
#[expect(
    clippy::print_stderr,
    reason = "the sweep narrates its progress and names the report it wrote; a test binary has no other sink"
)]
async fn corpus_run_sweep() {
    let root = corpus_root();
    if !has_corpus(&root) {
        eprintln!("skipping: corpus not fetched (scripts/corpus-fetch.sh)");
        return;
    }
    if !testkit::is_docker_ready().await {
        return;
    }

    let source = Arc::new(GitActionSource::new(root.join(".actions-cache")));
    let manifests: Arc<dyn ActionSource> = source.clone();

    let filter = env::var("PETRI_SWEEP_FILTER").unwrap_or_default();
    let jobs = usize::try_from(env_num("PETRI_SWEEP_JOBS", 4))
        .expect("PETRI_SWEEP_JOBS names a workflow parallelism that fits a usize");
    let timeout = Duration::from_secs(env_num("PETRI_SWEEP_TIMEOUT", 900));

    let pins = corpus_pins(&root);
    let event_mode = env::var("PETRI_SWEEP_EVENT").unwrap_or_default();
    let platform = env::var("PETRI_SWEEP_PLATFORM")
        .ok()
        .filter(|p| !p.is_empty());
    // `runner.arch` is the architecture the containers actually run: the
    // host's, unless a platform is forced.
    let runner_arch = match platform.as_deref() {
        Some(p) if p.ends_with("/amd64") => "X64",
        Some(p) if p.ends_with("/arm64") => "ARM64",
        _ => frontend_gha::runner_arch(consts::ARCH),
    };

    // Lower everything serially first: it fills the action caches while the
    // classification decides what runs. Excluded files (Windows/macOS,
    // callee-only, broken upstream) leave the denominator exactly as REPORT.md
    // leaves them.
    let mut records: Vec<RunRecord> = Vec::new();
    let mut queue: Vec<(usize, Artifact, bool)> = Vec::new();
    for (repo, repo_root, file) in workflows(&root) {
        let (outcome, graph) = lower_one(&repo, &repo_root, &file, Some(&manifests));
        if outcome.is_out_of_scope() || outcome.is_broken_upstream() || outcome.is_callee_only() {
            continue;
        }
        if !filter.is_empty() && !format!("{repo}/{}", outcome.file).contains(&filter) {
            continue;
        }
        let record = RunRecord {
            repo:   repo.clone(),
            file:   outcome.file.clone(),
            result: RunResult::NotLowered {
                features: outcome.unsupported_features(),
            },
        };
        if let (Class::Clean | Class::Warnings, Some(mut artifact)) = (outcome.class, graph) {
            artifact.map_graphs(|graph| {
                prepare(
                    graph,
                    &repo,
                    &outcome.file,
                    pins.get(&repo).map(String::as_str),
                    &repo_root,
                    platform.as_deref(),
                    runner_arch,
                );
            });
            // A reusable file run standalone has no caller to supply its
            // declared inputs; a firing-environment failure there is
            // caller-coupled, not a gap. (The word in the file is the signal:
            // hybrid-trigger files whose env broke on absent inputs would
            // break on GitHub's own non-call triggers too.)
            let text = fs::read_to_string(&file).unwrap_or_default();
            let caller_coupled = text.contains("workflow_call");
            // The workflow fires as its own primary trigger with a simulated
            // payload (fields real, resources fabricated), unless the
            // operator restores the bare dispatch. A caller-coupled file
            // keeps standalone semantics: no fabricated caller.
            if !caller_coupled && event_mode != "dispatch" {
                let sha = pins
                    .get(&repo)
                    .map_or("0000000000000000000000000000000000000000", String::as_str);
                let branch = default_branch(&repo_root).unwrap_or_else(|| "main".to_string());
                if let Some((name, payload)) = runs::simulated_event(&text, &repo, sha, &branch)
                    && let Some(github) = artifact.graph.params.get_mut("github")
                {
                    github["event_name"] = json!(name);
                    github["event"] = payload;
                }
            }
            queue.push((records.len(), artifact, caller_coupled));
        }
        records.push(record);
    }
    let to_run = queue.len();
    eprintln!(
        "sweep: {} in scope, {to_run} to run, {} not lowered",
        records.len(),
        records.len() - to_run
    );

    let semaphore = Arc::new(Semaphore::new(jobs));
    let mut set = JoinSet::new();
    for (slot, graph, caller_coupled) in queue {
        // The permit is acquired inside the task: every task spawns at once and
        // the completion loop below drains while work runs. Acquiring here
        // would stall spawning on the permits, and the first progress line
        // waited until nearly the whole sweep had finished.
        let semaphore = semaphore.clone();
        let source = source.clone();
        let repo = records[slot].repo.clone();
        let file = records[slot].file.clone();
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore open");
            let result = run_one(&repo, &file, graph, source, timeout, caller_coupled).await;
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
         `{}` (22.04/26.04 variants by label; the privileged dind flavor where \
         the graph drives a Docker engine), matrices capped to their first leg, \
         each workflow capped at {}s wall clock, parallelism {jobs}; the \
         full-image list runs on the ubuntu-latest capture, since its \
         workflows compile native gems against full-image packages. The \
         artifact and cache backends are live: each run gets the \
         distribution's ObjectService, cache and tool cache in the host \
         store, persistent across sweeps. Identity: \
         `github.sha` is the repo's pinned corpus commit (`corpus-pins.txt`) — the \
         sweep's analog of `default_params` reading HEAD — so `checkout` fetches \
         real state; {auth}. Each workflow fired as its own primary trigger with \
         a simulated event payload (fabricated resource #999999).",
        runs::RUNNER_IMAGE_2404,
        timeout.as_secs(),
    );
    let markdown = runs_report(&records, &note);
    let path = root.join("RUNS.md");
    fs::write(&path, &markdown).expect("write the runs report");
    eprintln!("wrote {}", path.display());
}

/// Sweep shape: scripts stubbed, host scopes containerized, matrices capped,
/// and the run parameters a host would fill — the corpus directory's slug as
/// the repository and its **pinned commit** as `github.sha`, the sweep's
/// analog of `default_params` reading the checkout's HEAD. A corpus dir is
/// workflows without a checkout, so the honest identity comes from
/// `corpus-pins.txt` and the fetch's recorded default branch; `checkout` then
/// fetches a commit that exists.
fn prepare(
    graph: &mut Graph,
    repo_slug: &str,
    file: &str,
    pin: Option<&str>,
    repo_root: &Path,
    platform: Option<&str>,
    runner_arch: &str,
) {
    // `PETRI_SWEEP_REAL`: comma-separated `repo/file` substrings whose `run:`
    // scripts stay REAL — the exploratory smoke for actual builds, always used
    // with a filter, never in the baseline sweep (real failures are the
    // project's business and would read as gaps).
    let real = env::var("PETRI_SWEEP_REAL").ok().is_some_and(|list| {
        let name = format!("{repo_slug}/{file}");
        list.split(',')
            .any(|pat| !pat.trim().is_empty() && name.contains(pat.trim()))
    });
    if !real {
        runs::stub_run_scripts(graph);
    }
    // A graph that drives a Docker engine gets the dind runner (and the
    // `--privileged` its daemon needs) where the 24.04 image would have been
    // picked — the only flavor the dind variant is built for. A workflow on
    // the full-image list outranks both: it needs the full runner's package
    // set, as on ubuntu-latest.
    let full = runs::is_full_image_workflow(repo_slug, file);
    let docker = runs::needs_docker(graph);
    runs::containerize(graph, platform, |requirements| {
        let image = battery_image(requirements);
        if full && image == runs::RUNNER_IMAGE_2404 {
            runs::runner_image_2404_full().to_string()
        } else if docker && image == runs::RUNNER_IMAGE_2404 {
            runs::RUNNER_IMAGE_2404_DIND.to_string()
        } else {
            image.to_string()
        }
    });
    if docker && !full {
        runs::privilege(graph, runs::RUNNER_IMAGE_2404_DIND);
    }
    runs::cap_expansions(graph);
    let mut github = identity::github_context(Some(repo_slug));
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
        json!({"os": "Linux", "arch": runner_arch, "name": "petri-sweep"}),
    );
    graph.params.insert("vars".into(), json!({}));
    // The corpus dir is the "repository" the substituted checkout materializes:
    // the real source tree at the pin, `.git` included — cloned and shaped
    // onto the run's branch, offline.
    graph.params.insert(
        "petri".into(),
        json!({ "repo": repo_root.display().to_string() }),
    );
}

/// `corpus-pins.txt`: `owner/repo <sha>` per line — the commit each corpus
/// repo was fetched at, which is the commit its workflows describe.
fn corpus_pins(corpus_root: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(corpus_root.join("../corpus-pins.txt")).unwrap_or_default();
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
    let text = fs::read_to_string(repo_root.join("PROVENANCE.md")).ok()?;
    let line = text.lines().find(|l| l.contains("default branch:"))?;
    let (_, rest) = line.split_once("default branch:")?;
    Some(rest.trim().trim_end_matches(')').to_string())
}

async fn run_one(
    repo: &str,
    file: &str,
    artifact: Artifact,
    source: Arc<GitActionSource>,
    timeout: Duration,
    caller_coupled: bool,
) -> RunResult {
    let failures = Arc::new(SweepFailures {
        caller_coupled,
        ..Default::default()
    });
    let label: String = format!("{repo}-{file}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = env::temp_dir()
        .join("petri-sweep")
        .join(format!("{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);

    let mut options = RunOptions::new(&dir);
    options.grace = Duration::from_secs(2);
    // Below the sweep's 90s post-cancel wait, or "wedged" is arithmetic: the
    // first cancel admits run_on_cancel cleanup and the kill only fires when
    // this grace expires — with the driver's 120s default, a run that WOULD
    // come down at ~125s was abandoned at 90s and read as wedged.
    options.cleanup_grace = Duration::from_secs(60);
    options.retention = Retention::Never;
    options.echo = false;
    // The sweep measures outcomes, not determinism; replay verification is the
    // acceptance battery's business.
    options.verify_replay = false;
    // The distribution's backends, so the sweep measures what the CLI ships:
    // each run gets its own ObjectService, with cache entries and the tool
    // cache in the host's persistent store — entries survive across sweeps,
    // so warm-cache effects are real, as they are on GitHub.
    let store = github_objects::default_store_dir();
    let tool_cache = github_objects::tool_cache_dir(&store);
    let _ = fs::create_dir_all(&tool_cache);
    let rt = Runtime::standard()
        .options(options)
        .step(github_actions::RunStep)
        .step(github_actions::ActionStep)
        .step(github_actions::DockerActionStep)
        .step(github_actions::DeferredActionStep)
        .step(github_actions::DeferredActionResultStep)
        .step(github_actions::DeferredActionPublishStep)
        .step(github_actions::DeferredActionPostStep)
        .step(github_actions::CheckoutStep)
        .step(github_actions::BackgroundStartStep)
        .step(github_actions::BackgroundCompleteStep)
        .step(github_actions::BackgroundPublishStep)
        .step(github_actions::BackgroundWaitStep)
        .step(github_actions::BackgroundCancelStep)
        .step(github_actions::WorkflowCallStep)
        .capability(ActionSourceCap(source.clone()))
        .capability(ActionManifestSourceCap(source))
        .capability(github_actions::ToolCacheCap(tool_cache))
        .secrets(MapSecrets::from_pairs(&[("GITHUB_TOKEN", &sweep_token())]));
    let rt = support::with_object_service(rt, Some(github_objects::cache_dir(&store)));

    let (handle_tx, handle_rx) = oneshot::channel();
    let graphs = HostRun::new(artifact.graph)
        .with_children(artifact.children)
        .observe(failures.clone());
    let mut run = tokio::spawn(async move {
        host::run_configured(&rt, graphs, |handle, _| {
            let _ = handle_tx.send(handle);
        })
        .await
    });
    let handle = handle_rx
        .await
        .expect("the host initializes its coordinator");

    let outcome = tokio::select! {
        joined = &mut run => Finished::Ran(Box::new(joined.expect("the coordinator task never panics").expect("the coordinator finishes"))),
        () = time::sleep(timeout) => {
            handle.cancel_root();
            if let Ok(joined) = time::timeout(Duration::from_secs(90), &mut run).await {
                let _ = joined.expect("the coordinator task never panics").expect("the coordinator finishes");
                Finished::TimedOut
            } else {
                // The cancel did not bring the run down. Abandon the task and
                // remove whatever containers the run dir's id names, since no
                // release will.
                run.abort();
                sweep_leftovers(&dir).await;
                Finished::Wedged
            }
        }
    };

    let result = match outcome {
        Finished::Wedged => RunResult::TimedOut { wedged: true },
        Finished::TimedOut => RunResult::TimedOut { wedged: false },
        Finished::Ran(report) => match report.status {
            ir::RunStatus::Success => RunResult::Pass,
            _ => failures
                .failed_children
                .lock()
                .expect("child failures lock")
                .first()
                .cloned()
                .unwrap_or_else(|| classify_failure(&report.state, caller_coupled)),
        },
    };
    let _ = fs::remove_dir_all(&dir);
    result
}

enum Finished {
    Ran(Box<ExecutionReport>),
    TimedOut,
    Wedged,
}

fn classify_failure(state: &EngineState, caller_coupled: bool) -> RunResult {
    let graph = state.graph();
    first_failure(
        state,
        &step_identities(graph),
        caller_coupled,
        &runs::stubbed_output_consumers(graph),
        &runs::dispatch_ref_checkouts(graph),
        &runs::empty_input_steps(graph),
        &runs::stashed_scripts(graph),
    )
}

#[derive(Default)]
struct SweepFailures {
    caller_coupled:  bool,
    pending:         Mutex<BTreeMap<ExecutionId, RunResult>>,
    failed_children: Mutex<Vec<RunResult>>,
}

#[async_trait::async_trait]
impl ExecutionObserver for SweepFailures {
    fn on_engine_record(&self, execution: ExecutionId, record: &EventRecord, state: &EngineState) {
        if matches!(&record.event, Event::StepFinished { firing, outcome, .. } if outcome.status.is_failure() && state.history().last().is_some_and(|finished| finished.firing == *firing))
        {
            self.pending
                .lock()
                .expect("execution failures lock")
                .entry(execution)
                .or_insert_with(|| classify_failure(state, self.caller_coupled));
        }
    }
    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        if let CoordinatorEvent::InvocationFinished { invocation, result } = &record.event
            && *invocation != InvocationId::ROOT
            && result.status == ir::RunStatus::Failed
            && let Some(failure) = self
                .pending
                .lock()
                .expect("execution failures lock")
                .remove(&result.final_execution)
        {
            self.failed_children
                .lock()
                .expect("child failures lock")
                .push(failure);
        }
    }
}

#[tokio::test]
async fn a_sweep_failure_names_the_failed_child_step() {
    let child = "on:\n  workflow_call: {}\njobs:\n  work:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 7\n";
    let caller = "on: push\njobs:\n  call:\n    uses: ./.github/workflows/child.yml\n";
    let files = support::files(&[(".github/workflows/child.yml", child)]);
    let failures = Arc::new(SweepFailures::default());
    let report = support::run_host(
        support::lower_ok_with(caller, &files).observe(failures.clone()),
        "sweep-child-failure",
    )
    .await;
    assert_eq!(report.status, ir::RunStatus::Failed);
    let failures = failures
        .failed_children
        .lock()
        .expect("child failures lock");
    let RunResult::Fail(failure) = &failures[0] else {
        panic!("the child failed")
    };
    assert_eq!(failure.node, "work/step-1");
    assert!(failure.message.contains('7'), "{}", failure.message);
}

/// The run's first failing record, read against the graph's step identities.
fn first_failure(
    state: &EngineState,
    identities: &BTreeMap<String, StepIdentity>,
    caller_coupled: bool,
    stub_consumers: &BTreeSet<String>,
    dispatch_refs: &BTreeSet<String>,
    empty_inputs: &BTreeSet<String>,
    scripts: &[(String, Option<String>)],
) -> RunResult {
    for record in state.history() {
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
        let lines = step_log(state, record.firing);
        let tail = display_tail(&lines);
        let expected = expected_reason(&identity, &class, !sweep_token().is_empty())
            .or_else(|| expected_from_log(&identity, &lines))
            .or_else(|| {
                (caller_coupled && class == FIRING_ENV_CLASS.as_str()).then(|| {
                    "requires its caller's inputs (a reusable workflow run standalone)".to_string()
                })
            })
            .or_else(|| {
                // A clone's record suffixes every segment (`job#0/step#1`);
                // the consumer set holds template names.
                let base = runs::clone_base(&record.name);
                stub_consumers
                    .contains(&base)
                    .then(|| "reads a stubbed script's output".to_string())
                    .or_else(|| {
                        // A ref built from an empty dispatch input names a ref
                        // only a real dispatch run has; no fetch of it can
                        // ever succeed locally.
                        dispatch_refs.contains(&base).then(|| {
                            "checks out a ref built from an empty dispatch input".to_string()
                        })
                    })
                    .or_else(|| {
                        // A step reading a defaultless declared input got the
                        // type's zero; when the file is a reusable one run
                        // standalone, the missing caller — not the runtime —
                        // is what left it empty.
                        (caller_coupled && empty_inputs.contains(&base)).then(|| {
                            "reads a caller input left empty (a reusable workflow run standalone)"
                                .to_string()
                        })
                    })
                    // A missing executable an earlier stubbed script in the
                    // same job would have installed is the stub's doing.
                    .or_else(|| runs::expected_from_stubbed_script(scripts, &base, &lines))
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
    let errors: Vec<String> = state.errors().iter().map(|e| format!("{e}")).collect();
    RunResult::Fail(FirstFailure {
        node:     "(run)".to_string(),
        step:     "(run)".to_string(),
        class:    String::new(),
        message:  if errors.is_empty() {
            format!(
                "run ended {:?} with no failing record",
                state.folded_status()
            )
        } else {
            format!("engine error: {}", errors.join(" | "))
        },
        tail:     Vec::new(),
        expected: None,
    })
}

/// The failing firing's whole log — what the classifiers read (the line that
/// names the cause can sit far above the end). With `PETRI_SWEEP_LOG` set, it
/// also goes to stderr — the dev loop for one workflow's failure.
#[expect(
    clippy::print_stderr,
    reason = "`PETRI_SWEEP_LOG` asks for the failing step's log on stderr; a test binary has no other sink"
)]
fn step_log(state: &EngineState, firing: ir::FiringId) -> Vec<String> {
    let lines: Vec<String> = state
        .log
        .events()
        .filter_map(|e| match e {
            Event::StepProgress {
                firing: f,
                ev: ir::StepEvent::Log { line, .. },
            } if *f == firing => Some(line.clone()),
            _ => None,
        })
        .collect();
    if env::var("PETRI_SWEEP_LOG").is_ok_and(|v| !v.is_empty()) {
        for line in &lines {
            eprintln!("    | {line}");
        }
    }
    lines
}

/// The report's window onto a failing log: its last lines.
fn display_tail(lines: &[String]) -> Vec<String> {
    let mut tail: Vec<String> = lines.iter().rev().take(4).rev().cloned().collect();
    // A step can keep printing after its error — deprecation-warning
    // continuations, stack frames — pushing the line that names the failure
    // out of the window. Keep it: the report reads it.
    if runs::error_line(&tail).is_none()
        && let Some(err) = runs::error_line(lines)
    {
        tail.insert(0, err.to_string());
    }
    tail
}

/// Remove the containers a wedged, abandoned run left behind: everything under
/// the run dir's recorded container-name prefix.
async fn sweep_leftovers(dir: &Path) {
    let Ok(id) = fs::read_to_string(dir.join(RUN_ID_FILE)) else {
        return;
    };
    let prefix = format!("petri-{}-", id.trim());
    let Ok(listed) = Command::new("docker")
        .args(["ps", "-a", "--format", "{{.Names}}"])
        .output()
        .await
    else {
        return;
    };
    for name in String::from_utf8_lossy(&listed.stdout).lines() {
        if name.starts_with(&prefix) {
            let _ = Command::new("docker")
                .args(["rm", "-f", name])
                .output()
                .await;
        }
    }
}
