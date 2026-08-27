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
    /// `on.workflow_call`: the file is reusable, with this call interface.
    pub call: Option<CallInterface<'a>>,
    /// `on.workflow_dispatch.inputs`, typed like call inputs.
    pub dispatch_inputs: Vec<InputDecl<'a>>,
    pub span: Span,
}

/// What `on.workflow_call` declares: the contract a caller binds against.
pub struct CallInterface<'a> {
    pub inputs: Vec<InputDecl<'a>>,
    /// Output name → its `value` expression, over the `jobs.*` context.
    pub outputs: Vec<(String, Node<'a>)>,
    /// Declared secret names (lowercased) with whether each is required.
    pub secrets: Vec<(String, bool)>,
}

/// One typed input — the same declaration for `workflow_call` and
/// `workflow_dispatch`, so both feed one validation path.
pub struct InputDecl<'a> {
    pub name: String,
    pub ty: InputType,
    pub required: bool,
    pub default: Option<Node<'a>>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputType {
    String,
    Boolean,
    Number,
    /// `choice`, with its declared options.
    Choice(Vec<String>),
    /// A deployment-environment name: locally, a string.
    Environment,
}

/// The caller's half of a reusable-workflow call: a job that is `uses:` plus
/// `with:` plus `secrets:`.
#[derive(Clone)]
pub struct WorkflowCall<'a> {
    pub uses: (String, Span),
    pub with: Vec<(String, Node<'a>)>,
    pub secrets: SecretsArg<'a>,
}

#[derive(Clone, Default)]
pub enum SecretsArg<'a> {
    /// No `secrets:` at all: only declared-optional secrets exist, empty.
    #[default]
    None,
    /// `secrets: inherit` — the callee sees the provider's names unchanged.
    Inherit,
    /// An explicit map: callee name → caller value (a `${{ secrets.X }}`).
    Map(Vec<(String, Node<'a>)>),
}

#[derive(Default, Clone, Copy)]
pub struct Defaults<'a> {
    pub shell: Option<Node<'a>>,
    pub working_directory: Option<Node<'a>>,
}

#[derive(Clone)]
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
    /// The job calls a reusable workflow instead of running steps.
    pub call: Option<WorkflowCall<'a>>,
}

/// The deployment environment a job targets. Its enforcement — approvals,
/// protection rules, wait timers, environment-scoped secrets — lives on GitHub's
/// servers, so a local run ignores it with a warning; the name, URL and
/// `deployment` flag are kept so the graph can say what the job would have
/// deployed to. Any value may be an expression, which stays as written: an
/// ignored field is never evaluated.
#[derive(Clone, Copy)]
pub struct Environment<'a> {
    pub name: Node<'a>,
    pub url: Option<Node<'a>>,
    pub deployment: Option<Node<'a>>,
}

#[derive(Clone, Copy)]
pub struct Strategy<'a> {
    /// Absent when `strategy:` only sets `fail-fast` or `max-parallel`, which is legal.
    pub matrix: Option<Node<'a>>,
    pub fail_fast: Option<Node<'a>>,
    pub max_parallel: Option<Node<'a>>,
}

#[derive(Clone)]
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
    let (call, dispatch_inputs) = read_triggers(top.get("on"), diags);

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
        call,
        dispatch_inputs,
        span: root.span(),
    })
}

/// The two triggers that declare inputs: `workflow_call` (the reusable-workflow
/// interface) and `workflow_dispatch`. Every other trigger stays metadata — a
/// local run fires the workflow directly.
#[allow(clippy::type_complexity)]
fn read_triggers<'a>(
    on: Option<Node<'a>>,
    diags: &mut Diagnostics,
) -> (Option<CallInterface<'a>>, Vec<InputDecl<'a>>) {
    let Some(m) = on.and_then(|on| on.as_mapping()) else {
        return (None, Vec::new());
    };
    let call = m.get("workflow_call").map(|wc| {
        let (inputs, outputs, secrets) = match wc.as_mapping() {
            None => (Vec::new(), Vec::new(), Vec::new()),
            Some(wm) => {
                wm.reject_unknown_keys(
                    &["inputs", "outputs", "secrets"],
                    diags,
                    "`on.workflow_call`",
                );
                let outputs = wm
                    .get("outputs")
                    .and_then(|o| o.as_mapping())
                    .map(|om| {
                        om.iter()
                            .filter_map(|(name, spec)| {
                                match spec.as_mapping().and_then(|sm| sm.get("value")) {
                                    Some(value) => Some((name.to_string(), value)),
                                    None => {
                                        diags.error(
                                            "gha.bad_call_output",
                                            spec.span(),
                                            format!("workflow output `{name}` needs a `value`"),
                                        );
                                        None
                                    }
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let secrets = wm
                    .get("secrets")
                    .and_then(|s| s.as_mapping())
                    .map(|sm| {
                        sm.iter()
                            .map(|(name, spec)| {
                                let required = spec
                                    .as_mapping()
                                    .and_then(|m| m.get("required"))
                                    .and_then(|r| r.as_scalar())
                                    .and_then(|s| s.as_bool())
                                    .unwrap_or(false);
                                (name.to_lowercase(), required)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    read_input_decls(wm.get("inputs"), diags),
                    outputs,
                    secrets,
                )
            }
        };
        CallInterface {
            inputs,
            outputs,
            secrets,
        }
    });
    let dispatch_inputs = m
        .get("workflow_dispatch")
        .and_then(|wd| wd.as_mapping())
        .map(|dm| read_input_decls(dm.get("inputs"), diags))
        .unwrap_or_default();
    (call, dispatch_inputs)
}

/// `inputs:` under `workflow_call` or `workflow_dispatch`: name, type, whether
/// required, and the default as written.
fn read_input_decls<'a>(node: Option<Node<'a>>, diags: &mut Diagnostics) -> Vec<InputDecl<'a>> {
    let Some(m) = node.and_then(|n| n.as_mapping()) else {
        return Vec::new();
    };
    m.iter()
        .map(|(name, spec)| {
            let sm = spec.as_mapping();
            let ty = match sm
                .as_ref()
                .and_then(|sm| sm.get("type"))
                .and_then(|t| t.as_str())
            {
                None | Some("string") => InputType::String,
                Some("boolean") => InputType::Boolean,
                Some("number") => InputType::Number,
                Some("environment") => InputType::Environment,
                Some("choice") => InputType::Choice(
                    sm.as_ref()
                        .and_then(|sm| sm.get("options"))
                        .and_then(|o| o.as_sequence())
                        .map(|seq| {
                            seq.iter()
                                .filter_map(|n| n.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
                Some(other) => {
                    diags.error(
                        "gha.bad_input",
                        spec.span(),
                        format!("input `{name}` has unknown type `{other}`"),
                    );
                    InputType::String
                }
            };
            InputDecl {
                name: name.to_string(),
                ty,
                required: sm
                    .as_ref()
                    .and_then(|sm| sm.get("required"))
                    .and_then(|r| r.as_scalar())
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false),
                default: sm.as_ref().and_then(|sm| sm.get("default")),
                span: spec.span(),
            }
        })
        .collect()
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
    let call = m.get("uses").and_then(|u| read_call(id, &m, u, diags));
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
        call,
    })
}

/// A job that is `uses: <workflow>@…`: the call reference, its `with:` and its
/// `secrets:`. The keys a call job cannot carry — GitHub's own rule — are
/// rejected here so a caller cannot half-configure a job that will never run
/// steps of its own.
fn read_call<'a>(
    id: &str,
    m: &Mapping<'a>,
    uses: Node<'a>,
    diags: &mut Diagnostics,
) -> Option<WorkflowCall<'a>> {
    let Some(reference) = uses.as_str() else {
        diags.error(
            "gha.bad_call",
            uses.span(),
            format!("job `{id}`: `uses` must be a workflow reference string"),
        );
        return None;
    };
    for key in [
        "steps",
        "runs-on",
        "container",
        "services",
        "env",
        "defaults",
        "outputs",
        "timeout-minutes",
        "continue-on-error",
        "environment",
    ] {
        if let Some(node) = m.get(key) {
            diags.error(
                "gha.bad_call",
                node.span(),
                format!("job `{id}` calls a reusable workflow, so it cannot have `{key}`"),
            );
        }
    }
    let secrets = match m.get("secrets") {
        None => SecretsArg::None,
        Some(s) if s.as_str() == Some("inherit") => SecretsArg::Inherit,
        Some(s) => match s.as_mapping() {
            Some(sm) => SecretsArg::Map(sm.iter().map(|(k, v)| (k.to_string(), v)).collect()),
            None => {
                diags.error(
                    "gha.bad_call",
                    s.span(),
                    format!("job `{id}`: `secrets` must be `inherit` or a mapping"),
                );
                SecretsArg::None
            }
        },
    };
    Some(WorkflowCall {
        uses: (reference.to_string(), uses.span()),
        with: env_entries(m.get("with"), diags, &format!("job `{id}` with")),
        secrets,
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
            m.get(key).and_then(|v| {
                v.expect_scalar(diags, &format!("`environment.{key}`"))
                    .map(|_| v)
            })
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

/// Convenience: is this mapping key present as any kind of node?
pub fn has(m: &Mapping<'_>, key: &str) -> bool {
    m.contains_key(key)
}
