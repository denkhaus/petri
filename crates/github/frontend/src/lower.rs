//! Workflow → HIR, per spec §12.
//!
//! This file is the pass's shape: [`Lowering`] and the plan it walks —
//! [`Frame`]s and [`Entry`]s — plus the [`lower`] driver and the pieces every
//! stage shares. The stages live in the submodules: [`frames`] flattens the
//! call graph and binds each frame's context, [`jobs`] builds each job's
//! bracket, body and edges, [`scope`] decides where a job runs and what its
//! steps see, [`steps`] lowers `run:` steps and their gates, and [`uses`]
//! resolves and lowers `uses:` steps.

mod frames;
mod jobs;
mod scope;
mod steps;
mod uses;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use frontend::FileSource;
use frontend::diag::{Diagnostic, Diagnostics, Lowered, Span};
use frontend::expr::lower::builtin;
use frontend::expr::parse;
use frontend::yaml::Node;
use ir::{BinOp, ExprId, ExprOrValue, GraphBuilder, NodeId, ScopeId, ValidationError, Value};

use crate::action::{ActionLocation, ActionSource, Phase, PinnedAction};
use crate::call::{self, CalleeSource};
use crate::composite::{DockerAction, NodeAction};
use crate::exprs::{LoweredScalar, SEP, SecretMap, Site, lower_scalar};
use crate::model::{Job, Step, Workflow};
use crate::runners::RunnerMap;

use self::frames::plan;
use self::uses::ResolveFailure;

struct JobNodes {
    scope: ScopeId,
    start: NodeId,
    done: NodeId,
    /// The last node of the step chain, which the expansion region ends at.
    last: NodeId,
    matrix: bool,
}

pub struct Lowering<'w, 'a> {
    b: GraphBuilder,
    diags: Diagnostics,
    wf: &'w Workflow<'a>,
    /// The planned frames — the root workflow and each inlined callee.
    frames: Vec<Frame<'w, 'a>>,
    files: &'w dyn FileSource,
    /// Where `uses: owner/repo@ref` actions come from. `None` rejects them.
    actions: Option<&'w dyn ActionSource>,
    /// Which `runs-on` labels place on this machine.
    runners: &'w RunnerMap,
    /// Remote actions resolved so far, by reference as written: the pin and the
    /// manifest text, or why not. A reference used by several steps resolves once.
    resolved: HashMap<String, Result<(PinnedAction, String), ResolveFailure>>,
    jobs: HashMap<String, JobNodes>,
    spans: HashMap<NodeId, Span>,
    /// The job in flight's per-leg `runs-on` resolutions: what each matrix leg
    /// resolved to, written by [`Lowering::expression_runs_on`] and taken back
    /// within the same [`Lowering::job_shell`] call for the job's `start` node
    /// meta. Never live across jobs.
    leg_runs_on: Option<Value>,
    /// The checkout's declared `github` identity — the repository slug from the
    /// origin remote where one exists — which placement guards evaluate
    /// against ([`crate::identity`]).
    github_identity: Value,
    /// One context per frame — the root workflow and each inlined callee —
    /// bound top-down before any node exists ([`Lowering::bind_frame`]).
    frame_ctx: Vec<FrameCtx>,
    /// The frame whose workflow `self.wf` currently is ([`Lowering::enter`]).
    current: usize,
}

/// One workflow being lowered: the root, or a callee inlined under a call job.
struct Frame<'w, 'a> {
    wf: &'w Workflow<'a>,
    /// Where this workflow's text came from — its own `./` references resolve
    /// against this.
    source: CalleeSource,
    /// `""` at the root; the call job's materialized id for a callee frame, so
    /// every node of the frame lives under `prefix/…`.
    prefix: String,
    /// The call that brought the frame in: (caller frame, its entry index).
    call: Option<CallEdge>,
    /// The frame sits inside a matrix call's expansion region: node names take
    /// `#index` suffixes, and further expansion heads cannot nest.
    in_expansion: bool,
    /// Callees below the root. Planning stops at [`call::MAX_DEPTH`] — the
    /// resolver already reported the cycle or the too-deep nest.
    depth: usize,
}

#[derive(Clone, Copy)]
struct CallEdge {
    caller: usize,
    entry: usize,
}

/// One materialized job of the flat plan: which frame it belongs to, the job
/// with its id and `needs` prefixed (borrowed as-is at the root, where there is
/// no prefix), and whether it is a workflow call.
struct Entry<'w, 'a> {
    frame: usize,
    job: Cow<'w, Job<'a>>,
    kind: EntryKind,
}

enum EntryKind {
    Job,
    /// A `uses:` job whose callee resolved: the frame its jobs were planned into.
    Call {
        callee: usize,
    },
}

/// The lowered context a frame's jobs read, bound once, top-down. The frame's
/// static facts (prefix, expansion-ness) stay on [`Frame`].
#[derive(Default)]
struct FrameCtx {
    /// The `inputs` context: a call's bound `with:`, or the root's typed
    /// run-parameter reads.
    inputs: Option<BTreeMap<String, ExprId>>,
    /// The subset of `inputs` whose values are known at lowering — what the
    /// per-leg `runs-on` resolver may read.
    static_inputs: BTreeMap<String, Value>,
    /// How `secrets.*` names map to the provider's.
    secrets: SecretMap,
    /// Inside a callee: the call's `start` node, ANDed into every job gate of
    /// the frame — when the call was skipped, nothing of the callee runs.
    call_start: Option<String>,
    /// Inside a callee: the call's exit join, which every job's `done` feeds.
    exit: Option<NodeId>,
    /// The frame's workflow was fetched from another repository, so its `./`
    /// step actions cannot resolve against this one.
    remote: bool,
}

/// A `uses:` step that contributes standalone nodes — a JavaScript action or a
/// Docker container action — resolved once for everything it contributes:
/// `pre` and `post` are placed away from the main node.
struct ActionPlan {
    /// Where the action's files are. `None` for `uses: docker://…`, which has
    /// no files at all.
    location: Option<ActionLocation>,
    kind: PlanKind,
    inputs: Vec<PlanInput>,
}

enum PlanKind {
    Node(NodeAction),
    Docker(DockerAction),
}

impl ActionPlan {
    fn has_pre(&self) -> bool {
        match &self.kind {
            PlanKind::Node(node) => node.pre.is_some(),
            PlanKind::Docker(docker) => docker.pre_entrypoint.is_some(),
        }
    }

    fn has_post(&self) -> bool {
        match &self.kind {
            PlanKind::Node(node) => node.post.is_some(),
            PlanKind::Docker(docker) => docker.post_entrypoint.is_some(),
        }
    }

    /// The phase's condition source (`pre-if` / `post-if`); the caller defaults
    /// an absent one to `always()`.
    fn phase_if(&self, phase: Phase) -> Option<&str> {
        let (pre_if, post_if) = match &self.kind {
            PlanKind::Node(node) => (node.pre_if.as_deref(), node.post_if.as_deref()),
            PlanKind::Docker(docker) => (docker.pre_if.as_deref(), docker.post_if.as_deref()),
        };
        match phase {
            Phase::Pre => pre_if,
            Phase::Post => post_if,
            Phase::Main => None,
        }
    }
}

struct PlanInput {
    name: String,
    /// The default's text, expressions and all; lowered where the step is.
    default: Option<String>,
    required: bool,
}

#[derive(Clone, Copy)]
struct ActionContext<'s, 'job, 'step> {
    job: &'s Job<'job>,
    step: &'s Step<'step>,
    scope: ScopeId,
    site: &'s Site,
    job_secret_env: &'s [(String, String)],
}

pub fn lower(
    wf: &Workflow<'_>,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    runners: &RunnerMap,
    mut diags: Diagnostics,
) -> Lowered {
    // Reusable workflows first: every callee fetched, parsed and cycle-checked
    // before a single node exists, so the passes below never fetch a workflow.
    let calls = call::resolve(wf, files, actions, &mut diags);
    let models = calls.models();
    let mut frames: Vec<Frame<'_, '_>> = vec![Frame {
        wf,
        source: CalleeSource::Root,
        prefix: String::new(),
        call: None,
        in_expansion: false,
        depth: 0,
    }];
    let mut entries: Vec<Entry<'_, '_>> = Vec::new();
    plan(&mut frames, &mut entries, &calls, &models, &mut diags);

    let mut lw = Lowering {
        b: GraphBuilder::bare(),
        diags,
        wf,
        frame_ctx: frames.iter().map(|_| FrameCtx::default()).collect(),
        frames,
        files,
        actions,
        runners,
        resolved: HashMap::new(),
        jobs: HashMap::new(),
        spans: HashMap::new(),
        leg_runs_on: None,
        github_identity: crate::identity::github_context(
            crate::identity::repository_slug(files).as_deref(),
        ),
        current: 0,
    };

    // Frame contexts top-down: a caller's inputs bind before its callee reads
    // them, and `plan` orders parents before children.
    for i in 0..lw.frames.len() {
        lw.bind_frame(i, &entries);
    }
    // Every job's scope and gate first, so `needs` can wire to them in any order.
    for e in &entries {
        lw.enter(e.frame);
        match e.kind {
            EntryKind::Job => lw.job_shell(&e.job),
            EntryKind::Call { callee } => lw.call_shell(e, callee),
        }
    }
    for e in &entries {
        lw.enter(e.frame);
        match e.kind {
            EntryKind::Job => lw.job_body(&e.job),
            EntryKind::Call { callee } => lw.call_body(e, callee),
        }
    }
    // Edge wiring reads each entry's own frame index; no `enter` needed.
    for e in &entries {
        lw.job_edges(e, &entries);
    }

    if lw.diags.has_errors() {
        return Lowered::rejected(lw.diags);
    }
    let builder = std::mem::replace(&mut lw.b, GraphBuilder::bare());
    let mut graph = builder.build();
    ir::normalize_loop_heads(&mut graph);
    let report = ir::check(&graph);
    for error in &report.errors {
        let span = lw.span_for(error);
        lw.diags.error(
            &format!("validate.{}", variant(error)),
            span,
            error.to_string(),
        );
    }
    for warning in &report.warnings {
        let span = lw.spans.get(&warning.at()).cloned().unwrap_or_default();
        let mut d = Diagnostic::warning(warning.code(), span, warning.to_string());
        if let Some(hint) = warning.hint() {
            d = d.with_hint(hint);
        }
        lw.diags.push(d);
    }
    Lowered::from_parts(graph, lw.diags)
}

impl<'w, 'a> Lowering<'w, 'a> {
    /// The job's site, as its own `env:`, `if:` and `outputs:` see it: `needs`
    /// known, matrix-ness known, the frame's inputs and secret map in scope, no
    /// steps yet. The `needs.*` context keys are the names as written — the
    /// frame prefix is stripped — while the values keep the prefixed node names.
    fn base_site(&self, job: &Job<'a>) -> Site {
        let ctx = &self.frame_ctx[self.current];
        let frame = &self.frames[self.current];
        let mut site = Site::new(&job.id);
        site.matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        site.workflow_inputs = ctx.inputs.clone();
        site.secrets = ctx.secrets.clone();
        site.in_expansion = frame.in_expansion;
        let strip = format!("{}{SEP}", frame.prefix);
        for (need, _) in &job.needs {
            let key = match frame.prefix.is_empty() {
                true => need.clone(),
                false => need.strip_prefix(&strip).unwrap_or(need).to_string(),
            };
            site.needs.insert(key, format!("{need}{SEP}done"));
        }
        site
    }

    /// The frame's `inputs` as the placement resolver may read them: values
    /// known at lowering, with a marked placeholder for each run-time one so a
    /// label that absorbs it names its input instead of placing.
    pub(crate) fn placement_inputs(&self) -> Value {
        let ctx = &self.frame_ctx[self.current];
        let mut inputs = serde_json::Map::new();
        if let Some(bound) = &ctx.inputs {
            for name in bound.keys() {
                let value = match ctx.static_inputs.get(name) {
                    Some(value) => value.clone(),
                    None => Value::String(format!("{}{name}", crate::runs_on::DYNAMIC_MARK)),
                };
                inputs.insert(name.clone(), value);
            }
        }
        Value::Object(inputs)
    }

    /// Make `frame` the one whose workflow the job passes read.
    fn enter(&mut self, frame: usize) {
        self.current = frame;
        self.wf = self.frames[frame].wf;
    }

    /// An `if:`: absent means `success()`; present is evaluated with GitHub's
    /// truthiness, and gets `success() &&` in front unless it names a status function.
    fn condition(
        &mut self,
        node: Option<Node<'_>>,
        site: &Site,
        at_step: bool,
        span: Span,
    ) -> Option<ExprId> {
        let Some(node) = node else {
            return Some(site.status_function(self.b.exprs(), "success", at_step));
        };
        let Some(scalar) = node.as_scalar() else {
            self.diags.error(
                "gha.bad_if",
                node.span(),
                "`if` must be a string or boolean",
            );
            return None;
        };
        if let Some(b) = scalar.as_bool() {
            let lit = self.b.exprs().lit(b);
            let success = site.status_function(self.b.exprs(), "success", at_step);
            return Some(self.b.exprs().binary(BinOp::And, success, lit));
        }
        let source = match if_expr_source(scalar.as_str()) {
            Ok(source) => source,
            Err(IfTemplateError::Mixed) => {
                self.diags.error(
                    "expr.mixed_condition",
                    node.span(),
                    "an `if` is one expression, bare or in a single `${{ }}`",
                );
                return None;
            }
            Err(IfTemplateError::Unterminated) => {
                self.diags
                    .error("expr.unterminated", node.span(), "unterminated `${{`");
                return None;
            }
        };
        self.condition_text(&source, site, at_step, span)
    }

    /// One condition's expression text (the body of an `if:`, a `pre-if`, a
    /// `post-if`) as a precondition: GitHub's truthiness, with `success() &&` in
    /// front unless it names a status function.
    fn condition_text(
        &mut self,
        source: &str,
        site: &Site,
        at_step: bool,
        span: Span,
    ) -> Option<ExprId> {
        let uses_status_function = parse(source)
            .map(|ast| names_status_function(&ast))
            .unwrap_or(false);
        let lowered = lower_scalar(
            &format!("${{{{ {source} }}}}"),
            span,
            site,
            at_step,
            false,
            self.b.exprs(),
            &mut self.diags,
        )?;
        let expr = match lowered {
            LoweredScalar::Expr(id) => id,
            LoweredScalar::Literal(v) => self.b.exprs().lit(v),
            LoweredScalar::Secret(_) => return None,
        };
        let truthy = builtin(self.b.exprs(), "loose_truthy", vec![expr]).ok()?;
        if uses_status_function {
            Some(truthy)
        } else {
            let success = site.status_function(self.b.exprs(), "success", at_step);
            Some(self.b.exprs().binary(BinOp::And, success, truthy))
        }
    }

    fn env_value(&mut self, node: &Node<'_>, site: &Site, at_step: bool) -> Option<EnvValue> {
        let text = match node.as_str() {
            Some(t) => t.to_string(),
            None => {
                // Non-string env values (numbers, booleans) are stringified.
                return Some(EnvValue::Plain(ExprOrValue::Value(Value::String(
                    scalar_text(*node),
                ))));
            }
        };
        match lower_scalar(
            &text,
            node.span(),
            site,
            at_step,
            true,
            self.b.exprs(),
            &mut self.diags,
        )? {
            LoweredScalar::Literal(v) => Some(EnvValue::Plain(ExprOrValue::Value(v))),
            LoweredScalar::Expr(id) => {
                // Env values are strings.
                let s = builtin(self.b.exprs(), "loose_string", vec![id]).ok()?;
                Some(EnvValue::Plain(ExprOrValue::Expr(s)))
            }
            LoweredScalar::Secret(name) => Some(EnvValue::Secret(name)),
        }
    }

    fn span_for(&self, error: &ValidationError) -> Span {
        let node = match error {
            ValidationError::LoopHeadMustJoinAny(n)
            | ValidationError::ZeroBudget(n)
            | ValidationError::UnboundedLoopBudget(n)
            | ValidationError::HirFieldInPlan(n)
            | ValidationError::EntryHasIncoming(n) => Some(*n),
            ValidationError::AlwaysNotLast { node, .. }
            | ValidationError::EmptyGroup { node, .. }
            | ValidationError::UnknownScope { node, .. }
            | ValidationError::ExitUnreachable { node, .. }
            | ValidationError::ExitNotPostdominator { node, .. }
            | ValidationError::BoundaryCrossing { node, .. } => Some(*node),
            _ => None,
        };
        node.and_then(|n| self.spans.get(&n).cloned())
            .unwrap_or_else(|| self.wf.span.clone())
    }
}

enum EnvValue {
    Plain(ExprOrValue),
    Secret(String),
}

/// A YAML scalar as the string GitHub would pass: text as written, other scalars
/// stringified, null empty.
fn scalar_text(node: Node<'_>) -> String {
    scalar_text_opt(node).unwrap_or_default()
}

/// [`scalar_text`], with a YAML null as `None`: an input whose `default:` is null
/// (or missing a value) has no default, and GitHub leaves it unset rather than
/// passing `"null"` — while an explicit `default: ''` is the empty string.
fn scalar_text_opt(node: Node<'_>) -> Option<String> {
    if node.as_scalar().is_some_and(|s| s.is_null()) {
        return None;
    }
    Some(match node.as_str() {
        Some(text) => text.to_string(),
        None => match node.to_json() {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        },
    })
}

/// Why an `if:` text holds no single expression. `condition` turns these into
/// diagnostics; `names_cleanup` reads them as false.
enum IfTemplateError {
    /// Text mixed with `${{ }}`, or more than one `${{ }}`.
    Mixed,
    /// An unterminated `${{`.
    Unterminated,
}

/// The one expression an `if:` string holds: the text itself, or the body of its
/// single `${{ }}`. Both readers of an `if:` go through here, so they cannot
/// disagree on what counts as one expression.
fn if_expr_source(text: &str) -> Result<String, IfTemplateError> {
    if !text.contains("${{") {
        return Ok(text.to_string());
    }
    match frontend::expr::split_template(text) {
        Ok(segments) => match segments.as_slice() {
            [frontend::expr::Segment::Expr { source, .. }] => Ok(source.clone()),
            _ => Err(IfTemplateError::Mixed),
        },
        Err(_) => Err(IfTemplateError::Unterminated),
    }
}

/// The status functions — GitHub's rule that a condition naming one does not
/// get `success() &&` in front, encoded once for the eager and lazy paths.
const STATUS_FUNCTIONS: &[&str] = &["success", "failure", "cancelled", "always"];

/// Whether the expression calls any status function.
fn names_status_function(ast: &frontend::expr::Expr) -> bool {
    ast.calls()
        .iter()
        .any(|c| STATUS_FUNCTIONS.contains(&c.to_lowercase().as_str()))
}

fn variant(error: &ValidationError) -> &'static str {
    match error {
        ValidationError::CycleWithoutBackEdge(_) => "cycle_without_back_edge",
        ValidationError::LoopHeadMustJoinAny(_) => "loop_head_must_join_any",
        ValidationError::ExitUnreachable { .. }
        | ValidationError::ExitNotPostdominator { .. }
        | ValidationError::BoundaryCrossing { .. } => "expansion_region",
        _ => "structure",
    }
}
