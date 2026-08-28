//! The corpus **run** battery's support: the run-time counterpart of the
//! lowering harness in the crate root.
//!
//! The sweep (`tests/runs.rs`, opt-in like the snapshot refresh) takes every
//! in-scope corpus workflow that lowers and runs it, measuring exactly the
//! runtime-tier surface — checkout, action staging and execution, the
//! artifact/cache backends, cross-job flow. Three transforms put a lowered
//! graph into sweep shape:
//!
//! - [`stub_run_scripts`]: every `run:` script becomes `true`. The corpus holds
//!   workflow files only — no sources, no `.git` — so real builds are impossible
//!   and beside the point; `uses:` steps stay real.
//! - [`containerize`]: every host scope is rewritten to a pinned runner image
//!   ([`battery_image`]), so corpus code never executes on the host.
//! - [`cap_expansions`]: every matrix is capped to its first leg. The metric is
//!   capability coverage, not leg multiplication.
//!
//! Classification separates *gap* from *server-coupled*: a first failure in a
//! step no local runner could ever satisfy (OIDC, GitHub App credentials,
//! third-party SaaS uploads, cross-run artifact downloads, repository secrets)
//! is an expected failure, kept out of the gap ranking — the same denominator
//! discipline that keeps REPORT.md honest.

use std::collections::BTreeMap;

use frontend_gha::action::{
    ACTION_KIND, ActionLocation, DOCKER_ACTION_KIND, PinnedAction, RUN_KIND,
};
use ir::{Expansion, Graph, RuntimeTarget, Value};
use smol_str::SmolStr;

/// The pinned battery images: flavor tags (`:slim`) move after maintained
/// builds, so the battery pins flavor+commit — reproducible runs, refreshed
/// deliberately. From lithoscomputer/sandbox-images; linux/amd64 only, so an
/// Apple-Silicon host runs them emulated (fine for the correctness metric,
/// noted for speed).
pub const RUNNER_IMAGE_2404: &str = "ghcr.io/lithoscomputer/ubuntu-24.04:slim-66c538cd3ef8";
pub const RUNNER_IMAGE_2204: &str = "ghcr.io/lithoscomputer/ubuntu-22.04:slim-66c538cd3ef8";
pub const RUNNER_IMAGE_2604: &str = "ghcr.io/lithoscomputer/ubuntu-26.04:slim-66c538cd3ef8";

/// The battery image for a scope's placement labels: the label's OS version
/// picks the Ubuntu release, defaulting to 24.04 (`ubuntu-latest`).
pub fn battery_image(requirements: &[SmolStr]) -> &'static str {
    for label in requirements {
        if label.contains("22.04") {
            return RUNNER_IMAGE_2204;
        }
        if label.contains("26.04") {
            return RUNNER_IMAGE_2604;
        }
    }
    RUNNER_IMAGE_2404
}

/// Stub every `github/run` script to `true`, keeping the step's gate and
/// placement so control flow is exercised without executing corpus code.
///
/// The step's own `env` and `working-directory` go with the script: the env can
/// name repository secrets no local run has, and the directory may only exist
/// because a real script would have created it — both would fail a step whose
/// work is now `true`.
pub fn stub_run_scripts(graph: &mut Graph) {
    for node in &mut graph.nodes {
        if node.step.kind.as_ref() != RUN_KIND {
            continue;
        }
        let Some(config) = node.step.config.as_object_mut() else {
            continue;
        };
        config.insert("run".into(), Value::from("true"));
        config.insert("shell".into(), Value::from("sh"));
        config.remove("shell_command");
        config.remove("env");
        config.remove("working_dir");
    }
}

/// Rewrite every host scope to a runner container, chosen per scope from its
/// placement labels. A workflow's own `container:` stays its own image — the
/// stand-in is only for scopes that would have run on the host. The explicit
/// `--platform` keeps the amd64-only images working on an arm64 daemon.
pub fn containerize(graph: &mut Graph, image_for: impl Fn(&[SmolStr]) -> String) {
    for scope in &mut graph.scopes {
        if !matches!(scope.runtime.target, RuntimeTarget::HostProcess) {
            continue;
        }
        let image = image_for(&scope.runtime.requirements);
        scope.runtime.target = RuntimeTarget::Container {
            image: SmolStr::new(&image),
            options: vec![SmolStr::new("--platform"), SmolStr::new("linux/amd64")],
            credentials: None,
        };
    }
}

/// Cap every expansion — static and expression matrices alike — to its first
/// leg, by rewriting the items expression to `[items[0]]`.
pub fn cap_expansions(graph: &mut Graph) {
    for index in 0..graph.nodes.len() {
        let Some(Expansion::ForEach { items, .. }) = &graph.nodes[index].expand else {
            continue;
        };
        let items = *items;
        let zero = graph.exprs.lit(0);
        let first = graph.exprs.index(items, zero);
        let capped = graph.exprs.array(vec![first]);
        if let Some(Expansion::ForEach { items, .. }) = &mut graph.nodes[index].expand {
            *items = capped;
        }
    }
}

/// What a graph node is, for reading a first failure: the `uses:` reference
/// behind it, or the step family when there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepIdentity {
    /// A remote action: the bare `owner/repo[/path]` and whether this call reads
    /// another run's artifacts (`download-artifact` with `run-id:`).
    Action { bare: String, cross_run: bool },
    /// A local (`./`) action step.
    LocalAction(String),
    /// A Docker container action (`docker://` or a Dockerfile action).
    DockerAction(String),
    /// A `run:` script — stubbed in the sweep, so a failure here is the
    /// harness's own.
    Run,
    /// Anything else (job start/done markers and other structure).
    Other(String),
}

impl StepIdentity {
    /// The label the report ranks by.
    pub fn label(&self) -> String {
        match self {
            StepIdentity::Action { bare, .. } => bare.clone(),
            StepIdentity::LocalAction(path) => format!("./{}", path.trim_start_matches("./")),
            StepIdentity::DockerAction(image) => image.clone(),
            StepIdentity::Run => "run:".to_string(),
            StepIdentity::Other(kind) => kind.clone(),
        }
    }
}

/// Node name → identity for every node in the graph. Runtime clones are named
/// `{name}#{index}`; [`identity_of`] strips that before looking here.
pub fn step_identities(graph: &Graph) -> BTreeMap<String, StepIdentity> {
    let mut out = BTreeMap::new();
    for node in &graph.nodes {
        let identity = match node.step.kind.as_ref() {
            ACTION_KIND => action_identity(&node.step.config),
            DOCKER_ACTION_KIND => docker_identity(&node.step.config),
            RUN_KIND => StepIdentity::Run,
            other => StepIdentity::Other(other.to_string()),
        };
        out.insert(node.name.to_string(), identity);
    }
    out
}

/// The identity for a firing-record name: exact match, else with the splice's
/// `#index` clone suffix stripped from each path segment.
pub fn identity_of<'a>(
    identities: &'a BTreeMap<String, StepIdentity>,
    record_name: &str,
) -> Option<&'a StepIdentity> {
    if let Some(found) = identities.get(record_name) {
        return Some(found);
    }
    let stripped: String = record_name
        .split('/')
        .map(strip_clone_suffix)
        .collect::<Vec<_>>()
        .join("/");
    identities.get(&stripped)
}

fn strip_clone_suffix(segment: &str) -> &str {
    match segment.rsplit_once('#') {
        Some((name, index)) if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => segment,
    }
}

fn action_identity(config: &Value) -> StepIdentity {
    let Some(location) = config
        .get("action")
        .and_then(|a| serde_json::from_value::<ActionLocation>(a.clone()).ok())
    else {
        return StepIdentity::Other(ACTION_KIND.to_string());
    };
    match location {
        ActionLocation::Pinned(pinned) => StepIdentity::Action {
            cross_run: reads_another_run(&pinned, config),
            bare: bare_reference(&pinned),
        },
        ActionLocation::Local { local } => StepIdentity::LocalAction(local),
    }
}

fn docker_identity(config: &Value) -> StepIdentity {
    match config.get("image") {
        Some(image) => {
            if let Some(registry) = image.get("registry").and_then(Value::as_str) {
                StepIdentity::DockerAction(registry.to_string())
            } else {
                StepIdentity::DockerAction("Dockerfile".to_string())
            }
        }
        None => StepIdentity::Other(DOCKER_ACTION_KIND.to_string()),
    }
}

/// `owner/repo[/path]` without the `@ref`.
fn bare_reference(pinned: &PinnedAction) -> String {
    let full = pinned.reference.to_string();
    full.split('@').next().unwrap_or(&full).to_string()
}

/// `download-artifact` with a `run-id:` input reads another workflow run's
/// artifacts over the REST API — a hosted-backend coupling no local run store
/// can answer.
fn reads_another_run(pinned: &PinnedAction, config: &Value) -> bool {
    if !bare_reference(pinned).ends_with("download-artifact") {
        return false;
    }
    config
        .get("inputs")
        .and_then(Value::as_object)
        .is_some_and(|inputs| inputs.contains_key("run-id"))
}

/// Actions whose work is inseparable from a hosted service: no local runner can
/// fix a failure here, so the sweep classifies it as expected rather than a gap.
pub const SERVER_COUPLED: &[(&str, &str)] = &[
    ("actions/attest-build-provenance", "OIDC attestation"),
    ("open-security-tools/ost-simple-sts", "OIDC token exchange"),
    ("google-github-actions/auth", "OIDC cloud auth"),
    ("aws-actions/configure-aws-credentials", "OIDC cloud auth"),
    ("azure/login", "OIDC cloud auth"),
    ("actions/create-github-app-token", "GitHub App credentials"),
    (
        "github/codeql-action/upload-sarif",
        "uploads to GitHub code scanning",
    ),
    ("codecov/codecov-action", "third-party SaaS upload"),
    ("coverallsapp/github-action", "third-party SaaS upload"),
    ("CodSpeedHQ/action", "third-party SaaS upload"),
];

/// The step-kind class a step reports when a `$secret` reference has no value.
/// Spelled here rather than imported: the lib half of this crate stays off the
/// runtime, and the string is a stable step-kind contract.
const SECRET_UNAVAILABLE: &str = "secret_unavailable";

/// Why this first failure is expected — server-coupled, not a gap — or `None`
/// when it measures a real runtime-tier gap.
pub fn expected_reason(identity: &StepIdentity, failure_class: &str) -> Option<String> {
    if failure_class == SECRET_UNAVAILABLE {
        return Some("needs a repository secret".to_string());
    }
    let StepIdentity::Action { bare, cross_run } = identity else {
        return None;
    };
    if *cross_run {
        return Some("cross-run artifact download (REST API)".to_string());
    }
    SERVER_COUPLED
        .iter()
        .find(|(action, _)| action == bare)
        .map(|(_, why)| why.to_string())
}

// ── The report ────────────────────────────────────────────────────────────

/// One workflow's sweep result.
#[derive(Clone, Debug)]
pub struct RunRecord {
    pub repo: String,
    pub file: String,
    pub result: RunResult,
}

#[derive(Clone, Debug)]
pub enum RunResult {
    /// The run reached `Success`.
    Pass,
    /// The run failed; this is its first failing step.
    Fail(FirstFailure),
    /// The wall-clock cap fired. `wedged` means even the cancel did not bring
    /// the run down and it was abandoned.
    TimedOut { wedged: bool },
    /// In scope but rejected at lowering — carried for the denominator, never run.
    NotLowered { features: Vec<String> },
}

/// The first failing step of a failed run.
#[derive(Clone, Debug)]
pub struct FirstFailure {
    /// The firing-record name, as recorded.
    pub node: String,
    /// The step behind it — [`StepIdentity::label`].
    pub step: String,
    /// The failure class the step reported (empty when unclassified).
    pub class: String,
    pub message: String,
    /// Present when the failure is server-coupled ([`expected_reason`]).
    pub expected: Option<String>,
}

impl RunResult {
    fn label(&self) -> String {
        match self {
            RunResult::Pass => "pass".to_string(),
            RunResult::Fail(f) if f.expected.is_some() => "expected failure".to_string(),
            RunResult::Fail(_) => "**fail**".to_string(),
            RunResult::TimedOut { wedged: false } => "**timeout**".to_string(),
            RunResult::TimedOut { wedged: true } => "**timeout (wedged)**".to_string(),
            RunResult::NotLowered { .. } => "not lowered".to_string(),
        }
    }
}

/// `RUNS.md`: the sweep's outcome per workflow plus first-failure classes
/// ranked — the run-time REPORT.md. `note` states this sweep's configuration
/// (image pin, caps, identity), written by the sweep that measured.
pub fn runs_report(records: &[RunRecord], note: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# Corpus run sweep\n");
    let _ = writeln!(
        out,
        "The run-time counterpart of REPORT.md: every in-scope workflow that lowers, \
         run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. \
         The outcome measures the runtime tier — checkout, action staging and \
         execution, artifact and cache backends, cross-job flow — not the corpus \
         projects' own builds.\n"
    );
    let _ = writeln!(out, "{note}\n");

    let total = records.len();
    let not_lowered = records
        .iter()
        .filter(|r| matches!(r.result, RunResult::NotLowered { .. }))
        .count();
    let ran = total - not_lowered;
    let pass = records
        .iter()
        .filter(|r| matches!(r.result, RunResult::Pass))
        .count();
    let expected = records
        .iter()
        .filter(|r| matches!(&r.result, RunResult::Fail(f) if f.expected.is_some()))
        .count();
    let gaps = records
        .iter()
        .filter(|r| matches!(&r.result, RunResult::Fail(f) if f.expected.is_none()))
        .count();
    let timeouts = records
        .iter()
        .filter(|r| matches!(r.result, RunResult::TimedOut { .. }))
        .count();
    let _ = writeln!(
        out,
        "{total} workflows in scope; {ran} lower and were run.\n"
    );
    let _ = writeln!(out, "| Result (of the {ran} run) | Count | Share |");
    let _ = writeln!(out, "|---|---|---|");
    let share = |n: usize| {
        if ran == 0 {
            0.0
        } else {
            100.0 * n as f64 / ran as f64
        }
    };
    for (label, n) in [
        ("passed", pass),
        ("**failed on a runtime-tier gap**", gaps),
        ("expected failure (server-coupled)", expected),
        ("**timed out**", timeouts),
    ] {
        let _ = writeln!(out, "| {label} | {n} | {:.0}% |", share(n));
    }

    // Gap classes ranked: what to build next, by how much it blocks.
    let mut gap_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut expected_classes: BTreeMap<String, usize> = BTreeMap::new();
    for record in records {
        if let RunResult::Fail(f) = &record.result {
            let key = if f.class.is_empty() {
                f.step.clone()
            } else {
                format!("{} · {}", f.step, f.class)
            };
            match &f.expected {
                Some(why) => {
                    *expected_classes
                        .entry(format!("{} · {why}", f.step))
                        .or_default() += 1
                }
                None => *gap_classes.entry(key).or_default() += 1,
            }
        }
    }
    let ranked = |classes: &BTreeMap<String, usize>| {
        let mut ranked: Vec<(String, usize)> =
            classes.iter().map(|(k, v)| (k.clone(), *v)).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
    };
    let _ = writeln!(out, "\n## First failures — gaps, ranked\n");
    if gap_classes.is_empty() {
        let _ = writeln!(out, "None.");
    } else {
        let _ = writeln!(out, "| First failing step · class | Workflows |");
        let _ = writeln!(out, "|---|---|");
        for (key, n) in ranked(&gap_classes) {
            let _ = writeln!(out, "| `{key}` | {n} |");
        }
    }
    let _ = writeln!(out, "\n## Expected failures — server-coupled, ranked\n");
    let _ = writeln!(
        out,
        "First failures no local runner can fix: OIDC, GitHub App and repository \
         secrets, third-party SaaS backends, cross-run artifact reads. Kept out of \
         the gap ranking so it never drowns in server-bound noise.\n"
    );
    if expected_classes.is_empty() {
        let _ = writeln!(out, "None.");
    } else {
        let _ = writeln!(out, "| First failing step · why | Workflows |");
        let _ = writeln!(out, "|---|---|");
        for (key, n) in ranked(&expected_classes) {
            let _ = writeln!(out, "| `{key}` | {n} |");
        }
    }

    // Per-workflow inventory.
    let _ = writeln!(out, "\n## Every workflow\n");
    let _ = writeln!(out, "| Repository | Workflow | Result | First failure |");
    let _ = writeln!(out, "|---|---|---|---|");
    for record in records {
        let detail = match &record.result {
            RunResult::Fail(f) => {
                let mut detail = format!("`{}` — {}", f.step, sanitize_cell(&f.message));
                if let Some(why) = &f.expected {
                    detail.push_str(&format!(" _({why})_"));
                }
                detail
            }
            RunResult::NotLowered { features } if !features.is_empty() => features
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "—".to_string(),
        };
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} |",
            record.repo,
            record.file.trim_start_matches(".github/workflows/"),
            record.result.label(),
            detail
        );
    }
    out
}

/// One Markdown table cell: no pipes, no newlines, bounded length.
fn sanitize_cell(text: &str) -> String {
    let mut cell: String = text
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
        .chars()
        .take(200)
        .collect();
    if cell.len() < text.len() {
        cell.push('…');
    }
    cell
}

#[cfg(test)]
mod tests {
    use super::*;
    use frontend_gha::action::ActionRef;
    use frontend_gha::load;
    use ir::Expr;

    fn lower(text: &str) -> Graph {
        let lowered = load(".github/workflows/test.yml", text, &frontend::NoFiles);
        lowered.graph.expect("the test workflow lowers")
    }

    #[test]
    fn stubbed_scripts_lose_env_shell_and_cwd() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 build:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   steps:\n\
             \x20     - run: import sys\n\
             \x20       shell: python\n\
             \x20       working-directory: sub\n\
             \x20       env: { A: b }\n",
        );
        stub_run_scripts(&mut graph);
        let step = graph
            .nodes
            .iter()
            .find(|n| n.step.kind.as_ref() == RUN_KIND)
            .expect("a run node");
        let config = step.step.config.as_object().expect("an object config");
        assert_eq!(config.get("run"), Some(&Value::from("true")));
        assert_eq!(config.get("shell"), Some(&Value::from("sh")));
        assert!(!config.contains_key("shell_command"));
        assert!(!config.contains_key("env"));
        assert!(!config.contains_key("working_dir"));
    }

    #[test]
    fn host_scopes_containerize_by_label_and_container_jobs_keep_their_image() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 old:\n\
             \x20   runs-on: ubuntu-22.04\n\
             \x20   steps: [{run: echo}]\n\
             \x20 boxed:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   container: alpine:3.20\n\
             \x20   steps: [{run: echo}]\n",
        );
        containerize(&mut graph, |req| battery_image(req).to_string());
        let images: Vec<String> = graph
            .scopes
            .iter()
            .filter_map(|s| match &s.runtime.target {
                RuntimeTarget::Container { image, .. } => Some(image.to_string()),
                RuntimeTarget::HostProcess => None,
            })
            .collect();
        assert!(
            images.contains(&RUNNER_IMAGE_2204.to_string()),
            "{images:?}"
        );
        assert!(images.contains(&"alpine:3.20".to_string()), "{images:?}");
        assert!(
            !graph
                .scopes
                .iter()
                .any(|s| matches!(s.runtime.target, RuntimeTarget::HostProcess))
        );
    }

    #[test]
    fn expansions_cap_to_the_first_leg() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 build:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   strategy: { matrix: { v: [1, 2, 3] } }\n\
             \x20   steps: [{run: echo}]\n",
        );
        cap_expansions(&mut graph);
        let items = graph
            .nodes
            .iter()
            .find_map(|n| {
                let Expansion::ForEach { items, .. } = n.expand.as_ref()?;
                Some(*items)
            })
            .expect("the matrix expansion");
        // `[old[0]]`: a one-element array around an index into the old items.
        let Some(Expr::Array(elements)) = graph.exprs.get(items) else {
            panic!("capped items are an array literal");
        };
        assert_eq!(elements.len(), 1);
        assert!(matches!(
            graph.exprs.get(elements[0]),
            Some(Expr::Index(..))
        ));
    }

    fn pinned(reference: &str) -> ActionLocation {
        ActionLocation::Pinned(PinnedAction {
            reference: ActionRef::parse(reference).expect("a valid reference"),
            sha: "0123456789012345678901234567890123456789".into(),
        })
    }

    #[test]
    fn identities_read_the_reference_and_the_cross_run_input() {
        let plain = serde_json::json!({
            "action": serde_json::to_value(pinned("actions/download-artifact@v4")).unwrap(),
            "inputs": {"name": "dist"},
        });
        assert_eq!(
            action_identity(&plain),
            StepIdentity::Action {
                bare: "actions/download-artifact".to_string(),
                cross_run: false
            }
        );
        let cross = serde_json::json!({
            "action": serde_json::to_value(pinned("actions/download-artifact@v4")).unwrap(),
            "inputs": {"run-id": "123"},
        });
        let identity = action_identity(&cross);
        assert_eq!(
            expected_reason(&identity, "network"),
            Some("cross-run artifact download (REST API)".to_string())
        );
    }

    #[test]
    fn classification_separates_gap_from_server_coupled() {
        let checkout = StepIdentity::Action {
            bare: "actions/checkout".to_string(),
            cross_run: false,
        };
        assert_eq!(expected_reason(&checkout, "exit_status:1"), None);
        assert_eq!(
            expected_reason(&checkout, "secret_unavailable"),
            Some("needs a repository secret".to_string())
        );
        let oidc = StepIdentity::Action {
            bare: "actions/attest-build-provenance".to_string(),
            cross_run: false,
        };
        assert_eq!(
            expected_reason(&oidc, "exit_status:1"),
            Some("OIDC attestation".to_string())
        );
    }

    #[test]
    fn record_names_strip_the_clone_suffix() {
        let mut identities = BTreeMap::new();
        identities.insert("build/step".to_string(), StepIdentity::Run);
        assert!(identity_of(&identities, "build/step").is_some());
        assert!(identity_of(&identities, "build/step#2").is_some());
        assert!(identity_of(&identities, "build#0/step#11").is_some());
        assert!(identity_of(&identities, "build/other").is_none());
    }
}
