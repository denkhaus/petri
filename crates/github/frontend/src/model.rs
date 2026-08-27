//! The shape of a GitHub Actions workflow file, read with positions.
//!
//! This is a *reading*, not a lowering: values that carry expressions stay as spanned
//! YAML nodes, so the lowering can report a bad `${{ }}` at the line it sits on.
//! Everything the engine cannot express is rejected here, loudly, with the package
//! that will add it named in the hint.

use frontend::diag::{Diagnostics, Span};
use frontend::yaml::{Document, Mapping, Node};

pub struct Workflow<'a> {
    pub name: Option<String>,
    pub env: Vec<(String, Node<'a>)>,
    pub defaults: Defaults<'a>,
    pub jobs: Vec<Job<'a>>,
    pub span: Span,
}

#[derive(Default, Clone, Copy)]
pub struct Defaults<'a> {
    pub shell: Option<Node<'a>>,
    pub working_directory: Option<Node<'a>>,
}

pub struct Job<'a> {
    pub id: String,
    pub span: Span,
    pub name: Option<Node<'a>>,
    pub needs: Vec<(String, Span)>,
    pub condition: Option<Node<'a>>,
    pub runs_on: Option<Node<'a>>,
    pub container: Option<Node<'a>>,
    pub env: Vec<(String, Node<'a>)>,
    pub defaults: Defaults<'a>,
    pub strategy: Option<Strategy<'a>>,
    pub continue_on_error: Option<Node<'a>>,
    pub timeout_minutes: Option<Node<'a>>,
    pub outputs: Vec<(String, Node<'a>)>,
    pub environment: Option<Environment<'a>>,
    pub steps: Vec<Step<'a>>,
    /// The job calls a reusable workflow (`uses:`). Already rejected; the rest of the
    /// reader stays quiet about what such a job is missing.
    pub reusable: bool,
}

/// The deployment environment a job targets. Its enforcement — approvals,
/// protection rules, wait timers, environment-scoped secrets — lives on GitHub's
/// servers, so a local run ignores it with a warning; the name, URL and
/// `deployment` flag are kept so the graph can say what the job would have
/// deployed to. Any value may be an expression, which stays as written: an
/// ignored field is never evaluated.
pub struct Environment<'a> {
    pub name: Node<'a>,
    pub url: Option<Node<'a>>,
    pub deployment: Option<Node<'a>>,
}

pub struct Strategy<'a> {
    /// Absent when `strategy:` only sets `fail-fast` or `max-parallel`, which is legal.
    pub matrix: Option<Node<'a>>,
    pub fail_fast: Option<Node<'a>>,
    pub max_parallel: Option<Node<'a>>,
}

pub struct Step<'a> {
    /// Position in the job, 0-based. Steps without an `id` are named from it.
    pub index: usize,
    pub id: Option<String>,
    pub span: Span,
    pub name: Option<Node<'a>>,
    pub condition: Option<Node<'a>>,
    pub run: Option<Node<'a>>,
    pub uses: Option<(String, Span)>,
    pub with: Vec<(String, Node<'a>)>,
    pub env: Vec<(String, Node<'a>)>,
    pub shell: Option<Node<'a>>,
    pub working_directory: Option<Node<'a>>,
    pub continue_on_error: Option<Node<'a>>,
    pub timeout_minutes: Option<Node<'a>>,
}

impl Step<'_> {
    /// The name this step's node carries: its `id`, or a positional one.
    pub fn node_name(&self) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => format!("step-{}", self.index + 1),
        }
    }
}

const TOP_KEYS: &[&str] = &[
    "name",
    "run-name",
    "on",
    "env",
    "defaults",
    "concurrency",
    "permissions",
    "jobs",
];
const JOB_KEYS: &[&str] = &[
    "name",
    "needs",
    "if",
    "runs-on",
    "container",
    "services",
    "env",
    "defaults",
    "strategy",
    "continue-on-error",
    "timeout-minutes",
    "outputs",
    "steps",
    "permissions",
    "environment",
    "concurrency",
    "uses",
    "with",
    "secrets",
];
const STEP_KEYS: &[&str] = &[
    "id",
    "name",
    "if",
    "run",
    "uses",
    "with",
    "env",
    "shell",
    "working-directory",
    "continue-on-error",
    "timeout-minutes",
    "background",
    "wait",
    "wait-all",
];

/// Read a workflow. Returns `None` only when the document is not a workflow at all;
/// individual rejections are diagnostics and reading continues past them so one pass
/// reports everything.
pub fn read<'a>(doc: &'a Document, diags: &mut Diagnostics) -> Option<Workflow<'a>> {
    let root = doc.root();
    let top = root.expect_mapping(diags, "a workflow")?;
    top.reject_unknown_keys(TOP_KEYS, diags, "the workflow");

    if let Some(c) = top.get("concurrency") {
        warn_concurrency(c, diags);
    }
    if let Some(p) = top.get("permissions") {
        diags.warning(
            "ignored.permissions",
            p.span(),
            "`permissions` configures the GitHub token and has no effect on a local run",
        );
    }
    if let Some(on) = top.get("on") {
        reject_unsupported_triggers(on, diags);
    }

    let name = top.get("name").and_then(|n| n.as_str()).map(str::to_string);
    let env = env_entries(top.get("env"), diags, "workflow `env`");
    let defaults = read_defaults(top.get("defaults"), diags);

    let mut jobs = Vec::new();
    match top.get("jobs") {
        None => diags.error("gha.no_jobs", top.span(), "a workflow needs `jobs:`"),
        Some(jobs_node) => {
            if let Some(mapping) = jobs_node.expect_mapping(diags, "`jobs`") {
                for (id, job) in mapping.iter() {
                    if let Some(job) = read_job(id, job, diags) {
                        jobs.push(job);
                    }
                }
            }
        }
    }

    Some(Workflow {
        name,
        env,
        defaults,
        jobs,
        span: root.span(),
    })
}

fn read_job<'a>(id: &str, node: Node<'a>, diags: &mut Diagnostics) -> Option<Job<'a>> {
    let m = node.expect_mapping(diags, &format!("job `{id}`"))?;
    m.reject_unknown_keys(JOB_KEYS, diags, &format!("job `{id}`"));

    // The rejection set, each with where it is headed.
    if let Some(c) = m.get("concurrency") {
        warn_concurrency(c, diags);
    }
    if let Some(s) = m.get("services") {
        diags.unsupported(
            "services",
            s.span(),
            format!("job `{id}` uses service containers"),
            "service containers are v2; run the service from a step for now",
        );
    }
    if let Some(u) = m.get("uses") {
        diags.unsupported(
            "workflow_call",
            u.span(),
            format!("job `{id}` calls a reusable workflow"),
            "reusable workflows are v2; inline the called workflow's jobs",
        );
    }
    let environment = m
        .get("environment")
        .and_then(|e| read_environment(id, e, diags));
    if let Some(p) = m.get("permissions") {
        diags.warning(
            "ignored.permissions",
            p.span(),
            "`permissions` configures the GitHub token and has no effect on a local run",
        );
    }

    let needs = match m.get("needs") {
        None => Vec::new(),
        Some(n) => {
            if let Some(one) = n.as_str() {
                vec![(one.to_string(), n.span())]
            } else if let Some(seq) = n.as_sequence() {
                seq.iter()
                    .filter_map(|item| item.as_str().map(|s| (s.to_string(), item.span())))
                    .collect()
            } else {
                diags.error(
                    "gha.bad_needs",
                    n.span(),
                    "`needs` must be a job id or a list of job ids",
                );
                Vec::new()
            }
        }
    };

    let mut steps = Vec::new();
    match m.get("steps") {
        None if m.contains_key("uses") => {}
        None => diags.error(
            "gha.no_steps",
            m.span(),
            format!("job `{id}` has no `steps`"),
        ),
        Some(s) => {
            if let Some(seq) = s.expect_sequence(diags, "`steps`") {
                for (index, step) in seq.iter().enumerate() {
                    if let Some(step) = read_step(id, index, step, diags) {
                        steps.push(step);
                    }
                }
            }
        }
    }

    let strategy = m.get("strategy").and_then(|s| {
        let sm = s.expect_mapping(diags, "`strategy`")?;
        sm.reject_unknown_keys(
            &["matrix", "fail-fast", "max-parallel"],
            diags,
            "`strategy`",
        );
        Some(Strategy {
            matrix: sm.get("matrix"),
            fail_fast: sm.get("fail-fast"),
            max_parallel: sm.get("max-parallel"),
        })
    });

    Some(Job {
        id: id.to_string(),
        span: node.span(),
        name: m.get("name"),
        needs,
        condition: m.get("if"),
        runs_on: m.get("runs-on"),
        container: m.get("container"),
        env: env_entries(m.get("env"), diags, &format!("job `{id}` env")),
        defaults: read_defaults(m.get("defaults"), diags),
        strategy,
        continue_on_error: m.get("continue-on-error"),
        timeout_minutes: m.get("timeout-minutes"),
        outputs: env_entries(m.get("outputs"), diags, &format!("job `{id}` outputs")),
        environment,
        steps,
        reusable: m.contains_key("uses"),
    })
}

/// `environment:` — a name, or a `{name, url}` mapping. The shape is validated
/// (a malformed value is still an error), the target is kept, and the job lowers
/// with a warning naming the semantics a local run does not have. Secrets scoped
/// to the environment stay unavailable: the secret provider has no environment
/// scope to serve them from.
fn read_environment<'a>(
    job: &str,
    node: Node<'a>,
    diags: &mut Diagnostics,
) -> Option<Environment<'a>> {
    let environment = if node.is_scalar() {
        Some(Environment {
            name: node,
            url: None,
            deployment: None,
        })
    } else if let Some(m) = node.as_mapping() {
        m.reject_unknown_keys(
            &["name", "url", "deployment"],
            diags,
            &format!("job `{job}` environment"),
        );
        let name = m.get("name");
        if name.is_none() {
            diags.error(
                "gha.bad_environment",
                node.span(),
                format!("job `{job}`: an `environment` mapping needs a `name`"),
            );
        }
        let scalar = |key: &str, diags: &mut Diagnostics| {
            m.get(key)
                .and_then(|v| v.expect_scalar(diags, &format!("`environment.{key}`")).map(|_| v))
        };
        let url = scalar("url", diags);
        let deployment = scalar("deployment", diags);
        name.and_then(|n| n.expect_scalar(diags, "`environment.name`").map(|_| n))
            .map(|name| Environment {
                name,
                url,
                deployment,
            })
    } else {
        diags.error(
            "gha.bad_environment",
            node.span(),
            format!("job `{job}`: `environment` must be a name or a `{{name, url}}` mapping"),
        );
        None
    };
    if environment.is_some() {
        diags.warning(
            "ignored.environment",
            node.span(),
            "`environment` is ignored: approvals, protection rules, wait timers, and \
             environment-scoped secrets and variables are GitHub server features and do not \
             apply to a local run",
        );
    }
    environment
}

pub fn read_step<'a>(
    job: &str,
    index: usize,
    node: Node<'a>,
    diags: &mut Diagnostics,
) -> Option<Step<'a>> {
    let m = node.expect_mapping(diags, &format!("step {} of job `{job}`", index + 1))?;
    m.reject_unknown_keys(
        STEP_KEYS,
        diags,
        &format!("step {} of job `{job}`", index + 1),
    );

    // Background steps and the `wait:` steps that join them. The IR can express this
    // — a background step is a fan-out, a `wait` is an `All` join — but that is a
    // lowering decision for the spec, so it is rejected specifically for now.
    let mut background = false;
    for key in ["background", "wait", "wait-all"] {
        if let Some(node) = m.get(key) {
            background = true;
            diags.unsupported(
                "step.background",
                node.span(),
                format!("step {} of job `{job}` uses `{key}:` (background steps)", index + 1),
                "a background step is a fan-out and its `wait:` an all-join, both of which the engine has; \
                 mapping them is a spec decision, not yet made",
            );
        }
    }

    let run = m.get("run");
    let uses = m
        .get("uses")
        .and_then(|u| u.as_str().map(|s| (s.to_string(), u.span())));
    match (run.is_some(), uses.is_some()) {
        (false, false) if background => {}
        (false, false) => diags.error(
            "gha.bad_step",
            node.span(),
            format!(
                "step {} of job `{job}` has neither `run` nor `uses`",
                index + 1
            ),
        ),
        (true, true) => diags.error(
            "gha.bad_step",
            node.span(),
            format!(
                "step {} of job `{job}` has both `run` and `uses`",
                index + 1
            ),
        ),
        _ => {}
    }

    Some(Step {
        index,
        id: m.get("id").and_then(|n| n.as_str()).map(str::to_string),
        span: node.span(),
        name: m.get("name"),
        condition: m.get("if"),
        run,
        uses,
        with: env_entries(m.get("with"), diags, "`with`"),
        env: env_entries(m.get("env"), diags, "step `env`"),
        shell: m.get("shell"),
        working_directory: m.get("working-directory"),
        continue_on_error: m.get("continue-on-error"),
        timeout_minutes: m.get("timeout-minutes"),
    })
}

fn read_defaults<'a>(node: Option<Node<'a>>, diags: &mut Diagnostics) -> Defaults<'a> {
    let Some(node) = node else {
        return Defaults::default();
    };
    let Some(m) = node.expect_mapping(diags, "`defaults`") else {
        return Defaults::default();
    };
    m.reject_unknown_keys(&["run"], diags, "`defaults`");
    let Some(run) = m.get("run").and_then(|r| r.as_mapping()) else {
        return Defaults::default();
    };
    run.reject_unknown_keys(&["shell", "working-directory"], diags, "`defaults.run`");
    Defaults {
        shell: run.get("shell"),
        working_directory: run.get("working-directory"),
    }
}

fn env_entries<'a>(
    node: Option<Node<'a>>,
    diags: &mut Diagnostics,
    what: &str,
) -> Vec<(String, Node<'a>)> {
    let Some(node) = node else { return Vec::new() };
    // GitHub allows `env: ${{ ... }}` as a whole; that is an expression-valued map we
    // cannot statically type, and rare enough to reject.
    if node.is_scalar() {
        diags.unsupported(
            "env.expression",
            node.span(),
            format!("{what} is a single expression rather than a mapping"),
            "write each variable as its own key",
        );
        return Vec::new();
    }
    let Some(m) = node.expect_mapping(diags, what) else {
        return Vec::new();
    };
    m.iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// `concurrency:` is cross-run mutual exclusion — one run against another. A
/// single local run has nothing to race, so the group is ignored, loudly: the rule
/// is real on GitHub, and the log should say it did not apply here. Cross-run
/// concurrency stays with the multi-run driver layer (decision D2).
fn warn_concurrency(node: Node<'_>, diags: &mut Diagnostics) {
    diags.warning(
        "ignored.concurrency",
        node.span(),
        "`concurrency` is ignored: cross-run mutual exclusion does not apply to a single local run",
    );
}

fn reject_unsupported_triggers(on: Node<'_>, diags: &mut Diagnostics) {
    let Some(m) = on.as_mapping() else { return };
    if let Some(wc) = m.get("workflow_call") {
        diags.unsupported(
            "workflow_call",
            wc.span(),
            "this workflow is reusable (`on: workflow_call`)",
            "reusable workflows are v2",
        );
    }
    if let Some(wd) = m.get("workflow_dispatch")
        && wd.as_mapping().is_some_and(|d| d.contains_key("inputs"))
    {
        diags.unsupported(
            "workflow_dispatch.inputs",
            wd.span(),
            "`workflow_dispatch` inputs",
            "dispatch inputs are v2; the `inputs` context cannot be populated for a local run",
        );
    }
}

/// `runs-on` labels the v1 local executor knows how to place.
///
/// These are GitHub-hosted labels the local executor places on this machine or a
/// Linux container. Third-party runner labels (`depot-*`, `namespace-profile-*`,
/// `*-16-core-*`) are rejected per label: placing them is a driver decision, not a
/// frontend guess.
pub const KNOWN_RUNS_ON: &[&str] = &[
    "ubuntu-latest",
    "ubuntu-slim",
    "ubuntu-26.04",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "ubuntu-20.04",
    "ubuntu-24.04-arm",
    "ubuntu-22.04-arm",
];

/// Convenience: is this mapping key present as any kind of node?
pub fn has(m: &Mapping<'_>, key: &str) -> bool {
    m.contains_key(key)
}
