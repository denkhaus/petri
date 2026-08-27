//! Workflow → HIR, per spec §12.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use frontend::FileSource;
use frontend::diag::{Diagnostic, Diagnostics, Lowered, Span};
use frontend::expr::lower::builtin;
use frontend::expr::parse;
use frontend::yaml::{Document, Node};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    BinOp, Budget, ExpandTarget, ExprId, ExprOrValue, GraphBuilder, NodeId, RuntimeSpec, Scope,
    ScopeId, StepRef, UnOp, ValidationError, Value,
};
use serde_json::{Map, json};
use smol_str::SmolStr;

use crate::action::{
    ACTION_KIND, ActionLocation, ActionRef, ActionSource, ActionSourceError, Phase, PinnedAction,
    RUN_KIND, STATE_OUTPUT_KEY,
};
use crate::call::{self, CallGraph, CalleeSource};
use crate::composite::{self, NodeAction, Runs, Uses};
use crate::exprs::{
    LoweredScalar, SEP, SecretMap, Site, config_value, lower_scalar, secret_sentinel,
    whole_value_secret,
};
use crate::gate::{self, Gate, GateOp};
use crate::inputs;
use crate::model::{CallInterface, Defaults, Job, SecretsArg, Step, Workflow, WorkflowCall};
use crate::runners::RunnerMap;
use crate::runs_on;

/// GitHub's default job timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(360 * 60);

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

struct CallEdge {
    caller: usize,
    entry: usize,
}

/// One materialized job of the flat plan: which frame it belongs to, the job
/// with its id and `needs` prefixed, and whether it is a workflow call.
struct Entry<'a> {
    frame: usize,
    job: Job<'a>,
    kind: EntryKind,
}

enum EntryKind {
    Job,
    /// A `uses:` job whose callee resolved: the frame its jobs were planned into.
    Call { callee: usize },
}

/// The lowered context a frame's jobs read, bound once, top-down.
#[derive(Default)]
struct FrameCtx {
    prefix: String,
    /// The `inputs` context: a call's bound `with:`, or the root's typed
    /// run-parameter reads.
    inputs: Option<BTreeMap<String, ExprId>>,
    /// How `secrets.*` names map to the provider's.
    secrets: SecretMap,
    in_expansion: bool,
    /// Inside a callee: the call's `start` node, ANDed into every job gate of
    /// the frame — when the call was skipped, nothing of the callee runs.
    call_start: Option<String>,
    /// Inside a callee: the call's exit join, which every job's `done` feeds.
    exit: Option<NodeId>,
    /// The frame's workflow was fetched from another repository, so its `./`
    /// step actions cannot resolve against this one.
    remote: bool,
}

/// A job under its frame's prefix: the id and every `needs` entry prefixed, so
/// names, `JobNodes` keys and wiring stay collision-free across inlined
/// workflows. The as-written names live on in each `Site`, which strips the
/// prefix for the `needs.*` context.
fn materialize<'a>(job: &Job<'a>, prefix: &str) -> Job<'a> {
    let mut out = job.clone();
    if prefix.is_empty() {
        return out;
    }
    out.id = format!("{prefix}{SEP}{}", job.id);
    out.needs = job
        .needs
        .iter()
        .map(|(need, span)| (format!("{prefix}{SEP}{need}"), span.clone()))
        .collect();
    out
}

/// Flatten the call graph into frames and materialized job entries, breadth
/// first from the root. A call whose callee failed to resolve (already a
/// diagnostic) becomes a plain empty job so its `needs` wiring stays intact
/// while the errors reject the workflow.
fn plan<'w, 'a>(
    frames: &mut Vec<Frame<'w, 'a>>,
    entries: &mut Vec<Entry<'a>>,
    calls: &'w CallGraph,
    models: &'w BTreeMap<String, (CalleeSource, Workflow<'a>)>,
    diags: &mut Diagnostics,
) {
    let mut i = 0;
    while i < frames.len() {
        let wf = frames[i].wf;
        let source = frames[i].source.clone();
        let prefix = frames[i].prefix.clone();
        let in_expansion = frames[i].in_expansion;
        let depth = frames[i].depth;
        for job in &wf.jobs {
            let materialized = materialize(job, &prefix);
            let has_matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
            let Some(call) = &job.call else {
                if has_matrix && in_expansion {
                    nested_matrix(diags, &job.span);
                }
                entries.push(Entry {
                    frame: i,
                    job: materialized,
                    kind: EntryKind::Job,
                });
                continue;
            };
            let callee = (depth < call::MAX_DEPTH)
                .then(|| calls.callee(&source, &call.uses.0))
                .flatten()
                .and_then(|(identity, _)| models.get(&identity));
            let Some((callee_source, callee_wf)) = callee else {
                entries.push(Entry {
                    frame: i,
                    job: materialized,
                    kind: EntryKind::Job,
                });
                continue;
            };
            if has_matrix && in_expansion {
                nested_matrix(diags, &job.span);
            }
            let child_expansion = in_expansion || has_matrix;
            frames.push(Frame {
                wf: callee_wf,
                source: callee_source.clone(),
                prefix: materialized.id.clone(),
                call: Some(CallEdge {
                    caller: i,
                    entry: entries.len(),
                }),
                in_expansion: child_expansion,
                depth: depth + 1,
            });
            entries.push(Entry {
                frame: i,
                job: materialized,
                kind: EntryKind::Call {
                    callee: frames.len() - 1,
                },
            });
        }
        i += 1;
    }
}

/// An expansion head inside an expansion region loses its own expansion — the
/// engine's clones never expand again — so a matrix under a matrix call is a
/// specific rejection rather than a wrong graph.
fn nested_matrix(diags: &mut Diagnostics, span: &Span) {
    diags.unsupported(
        "workflow_call.matrix",
        span.clone(),
        "a matrix inside a matrix workflow call",
        "the engine expands one region at a time: a matrix call's clones cannot expand again; \
         move the matrix to one side of the call",
    );
}

/// Why a remote action did not resolve.
#[derive(Clone)]
enum ResolveFailure {
    /// No [`ActionSource`] was given: the format is running without one.
    NoSource,
    /// The source said [`ActionSourceError::Unavailable`]: it does not serve this
    /// reference. Rejected like `NoSource`, scoped to the one reference; the
    /// reason, when the source knows one, tells the hint whether refreshing the
    /// source could ever help.
    Unavailable(Option<String>),
    Failed(String),
}

/// A `uses:` step that is a JavaScript action, resolved once for the nodes it
/// contributes: `pre` and `post` are placed away from the main one.
struct ActionPlan {
    location: ActionLocation,
    node: NodeAction,
    inputs: Vec<PlanInput>,
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
    let mut entries: Vec<Entry<'_>> = Vec::new();
    plan(&mut frames, &mut entries, &calls, &models, &mut diags);

    let mut lw = Lowering {
        b: GraphBuilder::bare(),
        diags,
        wf,
        files,
        actions,
        runners,
        resolved: HashMap::new(),
        jobs: HashMap::new(),
        spans: HashMap::new(),
        leg_runs_on: None,
        frame_ctx: frames.iter().map(|_| FrameCtx::default()).collect(),
        current: 0,
    };

    // Frame contexts top-down: a caller's inputs bind before its callee reads
    // them, and `plan` orders parents before children.
    for i in 0..frames.len() {
        lw.bind_frame(i, &frames, &entries);
    }
    // Every job's scope and gate first, so `needs` can wire to them in any order.
    for e in &entries {
        lw.enter(e.frame, &frames);
        match e.kind {
            EntryKind::Job => lw.job_shell(&e.job),
            EntryKind::Call { callee } => lw.call_shell(e, callee, &frames),
        }
    }
    for e in &entries {
        lw.enter(e.frame, &frames);
        match e.kind {
            EntryKind::Job => lw.job_body(&e.job),
            EntryKind::Call { callee } => lw.call_body(e, callee, &frames),
        }
    }
    for e in &entries {
        lw.enter(e.frame, &frames);
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
    // ── Jobs ───────────────────────────────────────────────────────────────

    /// The job's site, as its own `env:`, `if:` and `outputs:` see it: `needs`
    /// known, matrix-ness known, the frame's inputs and secret map in scope, no
    /// steps yet. The `needs.*` context keys are the names as written — the
    /// frame prefix is stripped — while the values keep the prefixed node names.
    fn base_site(&self, job: &Job<'a>) -> Site {
        let ctx = &self.frame_ctx[self.current];
        let mut site = Site::new(&job.id);
        site.matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        site.workflow_inputs = ctx.inputs.clone();
        site.secrets = ctx.secrets.clone();
        site.in_expansion = ctx.in_expansion;
        let strip = format!("{}{SEP}", ctx.prefix);
        for (need, _) in &job.needs {
            let key = match ctx.prefix.is_empty() {
                true => need.clone(),
                false => need.strip_prefix(&strip).unwrap_or(need).to_string(),
            };
            site.needs.insert(key, format!("{need}{SEP}done"));
        }
        site
    }

    /// Make `frame` the one whose workflow the job passes read.
    fn enter(&mut self, frame: usize, frames: &[Frame<'w, 'a>]) {
        self.current = frame;
        self.wf = frames[frame].wf;
    }

    /// Bind one frame's context: the root's inputs come from the run's
    /// parameters, a callee's from its call site — `with:` lowered in the
    /// caller's own site, `secrets:` folded through the caller's map so a
    /// nested `inherit` keeps renames intact.
    fn bind_frame(&mut self, i: usize, frames: &[Frame<'w, 'a>], entries: &[Entry<'a>]) {
        let frame = &frames[i];
        let Some(CallEdge { caller, entry }) = frame.call else {
            // The root: `workflow_call` and `workflow_dispatch` declarations
            // both bind from run parameters, through one typed path.
            let mut decls: Vec<&crate::model::InputDecl<'_>> = Vec::new();
            if let Some(interface) = &frame.wf.call {
                decls.extend(interface.inputs.iter());
            }
            decls.extend(frame.wf.dispatch_inputs.iter());
            let inputs = (!decls.is_empty())
                .then(|| inputs::bind_param_inputs(&decls, self.b.exprs(), &mut self.diags));
            self.frame_ctx[i] = FrameCtx {
                prefix: String::new(),
                inputs,
                secrets: SecretMap::Inherit,
                in_expansion: false,
                call_start: None,
                exit: None,
                remote: false,
            };
            return;
        };
        let call_job = &entries[entry].job;
        let call = call_job.call.as_ref().expect("a callee frame's entry is a call");
        // Bind in the caller's context.
        self.enter(caller, frames);
        let caller_site = self.base_site(call_job);
        let interface = frame.wf.call.as_ref();
        let decls = interface.map(|i| i.inputs.as_slice()).unwrap_or(&[]);
        let inputs = inputs::bind_call_inputs(
            decls,
            &call.with,
            &caller_site,
            &call.uses.0,
            &call.uses.1,
            self.b.exprs(),
            &mut self.diags,
        );
        let caller_secrets = self.frame_ctx[caller].secrets.clone();
        let secrets = self.bind_secrets(call, interface, &caller_secrets);
        self.frame_ctx[i] = FrameCtx {
            prefix: frame.prefix.clone(),
            inputs: Some(inputs),
            secrets,
            in_expansion: frame.in_expansion,
            call_start: Some(format!("{}{SEP}start", call_job.id)),
            exit: None,
            remote: matches!(frame.source, CalleeSource::Remote { .. })
                || self.frame_ctx[caller].remote,
        };
    }

    /// The callee's secret map: `inherit` keeps the caller's map (so renames
    /// survive nesting); an explicit block maps each declared name through the
    /// caller's values — every provided name must be declared, every value a
    /// whole `${{ secrets.NAME }}`, and a missing required secret is an error.
    fn bind_secrets(
        &mut self,
        call: &WorkflowCall<'a>,
        interface: Option<&CallInterface<'a>>,
        caller_secrets: &SecretMap,
    ) -> SecretMap {
        let provided = match &call.secrets {
            SecretsArg::Inherit => return caller_secrets.clone(),
            SecretsArg::None => Vec::new(),
            SecretsArg::Map(entries) => entries.clone(),
        };
        let declared: Vec<(String, bool)> = interface.map(|i| i.secrets.clone()).unwrap_or_default();
        for (name, node) in &provided {
            if !declared.iter().any(|(d, _)| d == &name.to_lowercase()) {
                self.diags.error(
                    "gha.bad_call",
                    node.span(),
                    format!("secret `{name}` is not declared by the called workflow"),
                );
            }
        }
        let mut map: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (name, required) in &declared {
            let value = provided
                .iter()
                .find(|(k, _)| k.to_lowercase() == *name)
                .map(|(_, v)| *v);
            match value {
                Some(node) => {
                    let text = node.as_str().unwrap_or("");
                    match whole_value_secret(text) {
                        Some(provider) => match caller_secrets.resolve(&provider) {
                            Ok(entry) => {
                                map.insert(name.clone(), entry);
                            }
                            Err(crate::exprs::UndeclaredSecret) => {
                                self.diags.error(
                                    "gha.undeclared_secret",
                                    node.span(),
                                    format!(
                                        "`secrets.{provider}` is not a secret this workflow call \
                                         provides"
                                    ),
                                );
                                map.insert(name.clone(), None);
                            }
                        },
                        None => {
                            self.diags.error(
                                "gha.bad_call",
                                node.span(),
                                format!(
                                    "secret `{name}` must be a whole `${{{{ secrets.NAME }}}}` \
                                     reference"
                                ),
                            );
                            map.insert(name.clone(), None);
                        }
                    }
                }
                None if *required => {
                    self.diags.error(
                        "gha.missing_secret",
                        call.uses.1.clone(),
                        format!("`{}` requires secret `{name}`", call.uses.0),
                    );
                }
                None => {
                    map.insert(name.clone(), None);
                }
            }
        }
        SecretMap::Explicit(map)
    }

    /// Inside a called workflow, every job gate carries the call's own
    /// admission: when the call was skipped or cancelled, nothing of the
    /// callee runs — not even `if: always()` — exactly as on GitHub.
    fn wrap_call_admission(&mut self, gate: Option<ExprId>, site: &Site) -> Option<ExprId> {
        let Some(call_start) = self.frame_ctx[self.current].call_start.clone() else {
            return gate;
        };
        let gate = gate?;
        let t = self.b.exprs();
        let record = site.need_record(t, &call_start);
        let status = t.field(record, "status");
        let ok = t.lit("success");
        let admitted = t.binary(BinOp::Eq, status, ok);
        Some(t.binary(BinOp::And, admitted, gate))
    }

    fn job_shell(&mut self, job: &Job<'a>) {
        let matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        let scope = self.scope_for(job);
        // `start` records whether it fired with its scope already cancelled: a
        // cleanup job admitted after a cancel says so here, and its steps'
        // `success()` reads it back (`Site::cancel_interrupt`).
        let cancelled = self.scope_cancelled_config();
        let start = self.b.add_node(
            &format!("{}{SEP}start", job.id),
            scope,
            StepRef::new(
                "noop",
                json!({ "job": job.id, "phase": "start", "cancelled": cancelled }),
            ),
        );
        // Every node fires after a polite cancel and its gate or precondition
        // decides — except an expansion head, which a cancelled scope never
        // splices. A matrix `start` carries the expansion, so it stays unflagged,
        // and so do its clones: a leg whose start had not fired when its scope
        // was cancelled never begins, exactly as GitHub cancels a queued
        // `fail-fast` leg before any of its steps.
        if !matrix {
            self.b.node_mut(start).run_on_cancel = true;
        }
        // Frontend facts preserved on `start`: the deployment target the run
        // ignores (the reader warned; values stay as written — an ignored field's
        // expressions are never evaluated), and what each matrix leg's `runs-on`
        // resolved to.
        let mut meta = Map::new();
        if let Some(environment) = &job.environment {
            let mut target = Map::new();
            target.insert("name".into(), json!(scalar_text(environment.name)));
            if let Some(url) = environment.url {
                target.insert("url".into(), json!(scalar_text(url)));
            }
            if let Some(deployment) = environment.deployment {
                target.insert("deployment".into(), json!(scalar_text(deployment)));
            }
            meta.insert("environment".into(), Value::Object(target));
        }
        if let Some(legs) = self.leg_runs_on.take() {
            meta.insert("runs_on".into(), legs);
        }
        if !meta.is_empty() {
            self.b.set_meta(start, Value::Object(meta));
        }
        self.spans.insert(start, job.span.clone());
        let done = self.b.add_node(
            &format!("{}{SEP}done", job.id),
            scope,
            StepRef::new("noop", Value::Null),
        );
        // Every `done` runs on cancel, unconditionally: it is a side-effect-free
        // fold, and a dependent's `always()` gate and `needs.J.*` reads need a
        // truthful summary in every cancel case.
        self.b.node_mut(done).run_on_cancel = true;
        self.spans.insert(done, job.span.clone());
        self.jobs.insert(
            job.id.clone(),
            JobNodes {
                scope,
                start,
                done,
                last: start,
                matrix,
            },
        );
    }

    /// A workflow call's bracket: `start` (the caller-side gate, and the
    /// expansion head when the call has a matrix), `exit` (the join every
    /// callee job's `done` feeds), and `done` (the fold dependents read, shaped
    /// exactly like a job's so `needs.<call>.outputs.*` works unchanged). The
    /// callee's jobs land between `start` and `exit`, so a matrix call expands
    /// the whole inlined workflow per leg.
    fn call_shell(&mut self, e: &Entry<'a>, callee: usize, frames: &[Frame<'w, 'a>]) {
        let job = &e.job;
        let matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        // The bracket's own scope: all three nodes are engine-side noops.
        let scope = self.b.add_scope(Scope::new(ScopeId::new(0)));
        let cancelled = self.scope_cancelled_config();
        let start = self.b.add_node(
            &format!("{}{SEP}start", job.id),
            scope,
            StepRef::new(
                "noop",
                json!({ "job": job.id, "phase": "start", "cancelled": cancelled }),
            ),
        );
        if !matrix {
            self.b.node_mut(start).run_on_cancel = true;
        }
        // What the call is, preserved on its start: the reference as written,
        // and the commit it pinned to.
        let call = job.call.as_ref().expect("a call entry");
        let mut target = Map::new();
        target.insert("uses".into(), json!(call.uses.0));
        if let CalleeSource::Remote { pinned } = &frames[callee].source {
            target.insert("sha".into(), json!(pinned.sha.as_str()));
        }
        self.b.set_meta(start, json!({ "call": target }));
        self.spans.insert(start, job.span.clone());

        let exit = self.b.add_node(
            &format!("{}{SEP}exit", job.id),
            scope,
            StepRef::new("noop", Value::Null),
        );
        self.b.node_mut(exit).run_on_cancel = true;
        self.spans.insert(exit, job.span.clone());
        let done = self.b.add_node(
            &format!("{}{SEP}done", job.id),
            scope,
            StepRef::new("noop", Value::Null),
        );
        self.b.node_mut(done).run_on_cancel = true;
        self.spans.insert(done, job.span.clone());
        self.jobs.insert(
            job.id.clone(),
            JobNodes {
                scope,
                start,
                done,
                last: exit,
                matrix,
            },
        );
        self.frame_ctx[callee].exit = Some(exit);
    }

    /// The call's behavior: the caller-side gate on `start`, the summary the
    /// `exit` join carries into `done` — the callee jobs' combined result
    /// (GitHub's call conclusion) plus the declared `workflow_call.outputs`,
    /// evaluated when every callee job of the leg is complete — and the matrix
    /// expansion over the whole bracket.
    fn call_body(&mut self, e: &Entry<'a>, callee: usize, frames: &[Frame<'w, 'a>]) {
        let job = &e.job;
        let Some((start, done, exit)) = self
            .jobs
            .get(&job.id)
            .map(|j| (j.start, j.done, j.last))
        else {
            return;
        };
        let mut site = self.base_site(job);
        let items = self.apply_strategy(job, &mut site);
        let gate = self.condition(job.condition, &site, false, job.span.clone());
        let gate = self.wrap_call_admission(gate, &site);
        if let Some(gate) = gate {
            self.b.set_precondition(start, gate);
        }

        // The callee's jobs, by their prefixed done nodes; the exit's site reads
        // them — suffix-aware inside a matrix call — through the `jobs` context
        // and the result terms below.
        let ctx = &self.frame_ctx[callee];
        let mut exit_site = Site::new(&job.id);
        exit_site.in_expansion = ctx.in_expansion || site.matrix;
        exit_site.workflow_inputs = ctx.inputs.clone();
        let mut dones = Vec::new();
        for callee_job in &frames[callee].wf.jobs {
            let done_name = format!("{}{SEP}{}{SEP}done", job.id, callee_job.id);
            exit_site
                .callee_jobs
                .insert(callee_job.id.clone(), done_name.clone());
            dones.push(done_name);
        }

        // Any callee job with the given result. Results are already folded per
        // job ('failure'/'cancelled'/'success'/'skipped'), so four tags suffice.
        let any = |lw: &mut Self, tag: &str| -> ExprId {
            let mut acc: Option<ExprId> = None;
            for name in &dones {
                let t = lw.b.exprs();
                let result = exit_site.need_result(t, name);
                let lit = t.lit(tag);
                let is = t.binary(BinOp::Eq, result, lit);
                acc = Some(match acc {
                    Some(a) => lw.b.exprs().binary(BinOp::Or, a, is),
                    None => is,
                });
            }
            acc.unwrap_or_else(|| lw.b.exprs().lit(false))
        };
        let any_failure = any(self, "failure");
        let any_cancelled = any(self, "cancelled");
        let any_success = any(self, "success");
        let t = self.b.exprs();
        let f = t.lit("failure");
        let c = t.lit("cancelled");
        let s = t.lit("success");
        let k = t.lit("skipped");
        let inner2 = t.cond(any_success, s, k);
        let inner1 = t.cond(any_cancelled, c, inner2);
        let result = t.cond(any_failure, f, inner1);

        // Declared outputs, lowered over the `jobs.*` context.
        let mut outputs: Vec<(String, ExprId)> = Vec::new();
        if let Some(interface) = &frames[callee].wf.call {
            for (name, node) in &interface.outputs {
                let text = node.as_str().unwrap_or("");
                if let Some(lowered) = lower_scalar(
                    text,
                    node.span(),
                    &exit_site,
                    false,
                    false,
                    self.b.exprs(),
                    &mut self.diags,
                ) {
                    let id = match lowered {
                        LoweredScalar::Literal(v) => self.b.exprs().lit(v),
                        LoweredScalar::Expr(id) => id,
                        LoweredScalar::Secret(_) => continue,
                    };
                    outputs.push((name.clone(), id));
                }
            }
        }
        let t = self.b.exprs();
        let outputs_obj = t.object(outputs.iter().map(|(k, v)| (k.as_str(), *v)).collect());
        let index = if site.matrix || exit_site.in_expansion {
            t.var("index")
        } else {
            t.lit(0)
        };
        let summary = t.object(vec![
            ("result", result),
            ("outputs", outputs_obj),
            ("index", index),
        ]);
        self.b
            .select(exit, vec![ir::Arm::always(done).with_map(summary)]);
        let fold = self.done_config();
        self.b.node_mut(done).step = StepRef::new("noop", fold);

        // Scheduling: the callee's rootless jobs begin when the call does.
        let roots: Vec<NodeId> = frames[callee]
            .wf
            .jobs
            .iter()
            .filter(|j| j.needs.is_empty())
            .filter_map(|j| {
                self.jobs
                    .get(&format!("{}{SEP}{}", job.id, j.id))
                    .map(|n| n.start)
            })
            .collect();
        if !roots.is_empty() {
            self.b.fan_out(start, &roots);
        }

        if let Some(items) = items {
            ir::parallel_for_each(
                &mut self.b,
                start,
                items,
                ExpandTarget::Subgraph { entry: start, exit },
                site.max_parallel,
                site.fail_fast,
            );
        }
    }

    fn scope_for(&mut self, job: &Job<'a>) -> ScopeId {
        let mut scope = Scope::new(ScopeId::new(0));
        scope.runtime = self.runtime_for(job);

        // Default environment GitHub gives every step, from the run parameters.
        let site = self.base_site(job);
        let github = |this: &mut Self, key: &str| -> ExprOrValue {
            let t = this.b.exprs();
            let ctx = t.var("github");
            let k = t.lit(key);
            let v = t.call("get_ci", vec![ctx, k]);
            let s = t.call("loose_string", vec![v]);
            ExprOrValue::Expr(s)
        };
        scope
            .env
            .insert("CI".into(), ExprOrValue::Value(json!("true")));
        scope
            .env
            .insert("GITHUB_ACTIONS".into(), ExprOrValue::Value(json!("true")));
        scope
            .env
            .insert("GITHUB_JOB".into(), ExprOrValue::Value(json!(job.id)));
        if let Some(name) = &self.wf.name {
            scope
                .env
                .insert("GITHUB_WORKFLOW".into(), ExprOrValue::Value(json!(name)));
        }
        for key in [
            "sha",
            "ref",
            "ref_name",
            "ref_type",
            "repository",
            "repository_owner",
            "actor",
            "event_name",
            "run_id",
            "run_number",
            "run_attempt",
            "server_url",
            "api_url",
            "base_ref",
            "head_ref",
        ] {
            let var = format!("GITHUB_{}", key.to_uppercase());
            scope.env.insert(SmolStr::new(&var), github(self, key));
        }
        for key in ["os", "arch", "name", "temp", "tool_cache"] {
            let t = self.b.exprs();
            let ctx = t.var("runner");
            let k = t.lit(key);
            let v = t.call("get_ci", vec![ctx, k]);
            let s = t.call("loose_string", vec![v]);
            scope.env.insert(
                SmolStr::new(format!("RUNNER_{}", key.to_uppercase())),
                ExprOrValue::Expr(s),
            );
        }

        // Workflow env, then job env on top. Secret refs cannot live in scope env
        // (there is nowhere for them to go but the process), so they are pushed down
        // into every step's env instead; `job_body` reads them back from `site`.
        for (key, node) in self.wf.env.iter().chain(job.env.iter()) {
            match self.env_value(node, &site, false) {
                Some(EnvValue::Plain(v)) => {
                    scope.env.insert(SmolStr::new(key), v);
                }
                Some(EnvValue::Secret(_)) => {
                    // Recorded in job_body via the same lookup; nothing to do here.
                }
                None => {}
            }
        }
        self.b.add_scope(scope)
    }

    fn runtime_for(&mut self, job: &Job<'a>) -> RuntimeSpec {
        let mut spec = RuntimeSpec::host_process();

        match job.runs_on {
            None if job.call.is_some() => {}
            None => {
                self.diags.error(
                    "gha.no_runs_on",
                    job.span.clone(),
                    format!("job `{}` has no `runs-on`", job.id),
                );
            }
            Some(node) => {
                let raw: Vec<runs_on::RawLabel> =
                    if let Some(label) = runs_on::RawLabel::from_node(node, true) {
                        vec![label]
                    } else if let Some(seq) = node.as_sequence() {
                        seq.iter()
                            .filter_map(|n| runs_on::RawLabel::from_node(n, false))
                            .collect()
                    } else if let Some(m) = node.as_mapping() {
                        // `runs-on: { group: …, labels: … }`
                        let mut out = Vec::new();
                        if let Some(labels) = m.get("labels") {
                            if let Some(one) = runs_on::RawLabel::from_node(labels, false) {
                                out.push(one);
                            } else if let Some(seq) = labels.as_sequence() {
                                out.extend(
                                    seq.iter()
                                        .filter_map(|n| runs_on::RawLabel::from_node(n, false)),
                                );
                            }
                        }
                        if let Some(group) = m.get("group") {
                            self.diags.unsupported(
                                "runs_on.group",
                                group.span(),
                                "`runs-on.group` names a runner group",
                                "runner groups are a GitHub-hosted concept; name a label instead",
                            );
                        }
                        out
                    } else {
                        Vec::new()
                    };
                if raw.iter().any(|l| l.text.contains("${{")) {
                    self.expression_runs_on(job, node, &raw, &mut spec);
                } else {
                    for label in raw {
                        self.check_label(&mut spec, &label.text, label.span, None);
                    }
                }
            }
        }

        if let Some(container) = job.container {
            let image = if let Some(s) = container.as_str() {
                Some((s.to_string(), container.span()))
            } else if let Some(m) = container.as_mapping() {
                for (key, span) in m.keys() {
                    match key {
                        "image" | "env" => {}
                        other => self.diags.unsupported(
                            &format!("container.{other}"),
                            span,
                            format!("`container.{other}`"),
                            "only `image` and `env` are mapped onto a container scope; `options` are engine flags, which the graph does not carry",
                        ),
                    }
                }
                m.get("image")
                    .and_then(|i| i.as_str().map(|s| (s.to_string(), i.span())))
            } else {
                None
            };
            match image {
                Some((image, span)) if image.contains("${{") => {
                    self.diags.unsupported(
                        "container.expression",
                        span,
                        "an expression-valued container image",
                        "the image is fixed per scope; use a literal",
                    );
                }
                Some((image, _)) => {
                    let requirements = std::mem::take(&mut spec.requirements);
                    spec = RuntimeSpec::container(&image);
                    spec.requirements = requirements;
                }
                None => self.diags.error(
                    "gha.bad_container",
                    container.span(),
                    "`container` needs an `image`",
                ),
            }
        }
        spec
    }

    /// `runs-on` carrying expressions: resolved now, once per matrix leg, as
    /// GitHub resolves it at queue time with that leg's `matrix` in scope. The
    /// legs come from the same combinators the engine expands with, so the leg
    /// set here and the leg set at run time cannot disagree. Each leg's labels
    /// go through the same placement policy as literal labels — a rejected leg
    /// names itself, and does not stop the others from resolving — and their
    /// union becomes the scope's requirements. Per-leg results are preserved on
    /// the job's `start` node ([`Self::job_shell`]).
    fn expression_runs_on(
        &mut self,
        job: &Job<'a>,
        node: Node<'_>,
        raw: &[runs_on::RawLabel],
        spec: &mut RuntimeSpec,
    ) {
        let matrix = job.strategy.as_ref().and_then(|s| s.matrix);
        let legs = match matrix {
            // No matrix: the expression still resolves, over an empty `matrix`.
            None => Some(vec![json!({})]),
            Some(matrix) => runs_on::static_legs(matrix),
        };
        let Some(legs) = legs else {
            self.diags.unsupported(
                "runs_on.expression",
                node.span(),
                format!(
                    "job `{}`: `runs-on` is an expression and the matrix is not static",
                    job.id
                ),
                "the matrix carries expressions, so its legs — and each leg's `runs-on` — are \
                 unknown before the run; use literal matrix values or a fixed label",
            );
            return;
        };
        let compiled = match runs_on::compile(raw) {
            Ok(compiled) => compiled,
            Err(failure) => {
                self.runs_on_failure(job, failure, None);
                return;
            }
        };
        let mut resolved = Vec::with_capacity(legs.len());
        for leg in &legs {
            match compiled.labels_for(leg) {
                Ok(labels) => {
                    let only: Vec<&str> = labels.iter().map(|(l, _)| l.as_str()).collect();
                    let against = matrix.is_some().then_some(leg);
                    for (label, span) in &labels {
                        self.check_label(spec, label, span.clone(), against);
                    }
                    resolved.push(json!({ "leg": leg, "labels": only }));
                }
                // One leg failing to resolve does not stop the others.
                Err(failure) => self.runs_on_failure(job, failure, Some(leg)),
            }
        }
        self.leg_runs_on = Some(Value::Array(resolved));
    }

    /// One label through the placement policy — the same policy for labels
    /// written literally and labels an expression resolved to. Windows and
    /// macOS are specific errors whatever the runner map says; every other
    /// label answers to the map ([`RunnerMap`]), and an unmapped one is an
    /// explicit rejection, never a silently skipped job. Whatever passes joins
    /// the scope's requirements — each label once, so a label shared by many
    /// legs is checked and reported once.
    fn check_label(
        &mut self,
        spec: &mut RuntimeSpec,
        label: &str,
        span: Span,
        leg: Option<&Value>,
    ) {
        if spec.requirements.iter().any(|r| r == label) {
            return;
        }
        // Built only on the diagnostic paths; the common path accepts the label.
        let place = || {
            leg.map(|l| format!(" (matrix leg {l})"))
                .unwrap_or_default()
        };
        let lowered = label.to_lowercase();
        if lowered.starts_with("windows") {
            self.diags.unsupported(
                "runs_on.windows",
                span,
                format!("`runs-on: {label}`{}", place()),
                "Windows runners are out of scope; the local executor emulates Linux runners",
            );
        } else if lowered.starts_with("macos") {
            self.diags.unsupported(
                "runs_on.macos",
                span,
                format!("`runs-on: {label}`{}", place()),
                "macOS runners are out of scope; the local executor emulates Linux runners",
            );
        } else if !self.runners.knows_lowered(&lowered) {
            self.diags.unsupported(
                "runs_on.unknown",
                span,
                format!(
                    "`runs-on: {label}`{} is not a label the runner map knows",
                    place()
                ),
                &format!(
                    "labels this machine places: {}. A third-party or self-hosted label naming a \
                     usable Linux environment can be added to the runner map — \
                     `PETRI_RUNNER_LABELS` for the shipped CLI, `GitHubActions::with_runners` in \
                     code",
                    self.runners.known().join(", ")
                ),
            );
        }
        spec.requirements.push(SmolStr::new(label));
    }

    /// Why a `runs-on` did not resolve at lowering, as the one
    /// `runs_on.expression` rejection with the specifics in the message.
    fn runs_on_failure(&mut self, job: &Job<'a>, failure: runs_on::Failure, leg: Option<&Value>) {
        let for_leg = leg
            .map(|l| format!(" for matrix leg {l}"))
            .unwrap_or_default();
        match failure {
            runs_on::Failure::RunTimeContext { name, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` reads `{name}`, which has no value before the run{for_leg}",
                    job.id
                ),
                "`runs-on` is resolved at lowering, where only `matrix` has a value; `github`, \
                 `needs` and `inputs` are run-time contexts — use a fixed label or matrix values",
            ),
            runs_on::Failure::Bad { message, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` cannot be resolved at lowering{for_leg}: {message}",
                    job.id
                ),
                "`runs-on` is resolved at lowering, per matrix leg; use matrix values, literals, \
                 and the documented functions over them",
            ),
            runs_on::Failure::NotLabels { got, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` evaluated to {got}{for_leg}, not a label or list of labels",
                    job.id
                ),
                "each leg's `runs-on` must resolve to a string or a list of strings",
            ),
        }
    }

    /// `strategy:` onto the site — `fail-fast`, `max-parallel`, the static leg
    /// count — and the expansion's items expression, shared by plain jobs and
    /// workflow calls.
    fn apply_strategy(&mut self, job: &Job<'a>, site: &mut Site) -> Option<ExprId> {
        let strategy = job.strategy.as_ref().filter(|s| s.matrix.is_some())?;
        let matrix_node = strategy.matrix.expect("filtered");
        let matrix_expr = self.matrix_expr(matrix_node, site);
        site.matrix_total = runs_on::static_legs(matrix_node).map(|legs| legs.len());
        site.fail_fast = match strategy
            .fail_fast
            .and_then(|n| n.as_scalar())
            .map(|s| (s.as_bool(), s.as_str().to_string()))
        {
            None => true,
            Some((Some(b), _)) => b,
            Some((None, text)) => {
                self.diags.unsupported(
                    "strategy.fail_fast.expression",
                    strategy.fail_fast.map(|n| n.span()).unwrap_or_default(),
                    format!("`fail-fast: {text}`"),
                    "use a literal true or false",
                );
                true
            }
        };
        site.max_parallel = match strategy
            .max_parallel
            .and_then(|n| n.as_scalar())
            .map(|s| (s.as_i64(), s.as_str().to_string()))
        {
            None => None,
            Some((Some(n), _)) if n >= 1 => Some(n as u32),
            Some((_, text)) => {
                self.diags.unsupported(
                    "strategy.max_parallel.expression",
                    strategy.max_parallel.map(|n| n.span()).unwrap_or_default(),
                    format!("`max-parallel: {text}`"),
                    "use a literal integer",
                );
                None
            }
        };
        matrix_expr.and_then(|m| crate::expr_lower::matrix_legs(self.b.exprs(), m).ok())
    }

    /// Steps, preconditions, config, and the matrix expansion.
    fn job_body(&mut self, job: &Job<'a>) {
        let shell = self
            .jobs
            .get(&job.id)
            .map(|j| (j.scope, j.start, j.done, j.matrix));
        let Some((scope, start, done, matrix)) = shell else {
            return;
        };

        // Secrets at workflow/job level are pushed down into every step.
        let mut site = self.base_site(job);
        let mut job_secret_env: Vec<(String, String)> = Vec::new();
        for (key, node) in self.wf.env.iter().chain(job.env.iter()) {
            // Lowered once already for the scope; here only to find the secrets, whose
            // diagnostics (if any) were reported then.
            let mut scratch = Diagnostics::new();
            let saved = std::mem::replace(&mut self.diags, scratch);
            let value = self.env_value(node, &site, false);
            scratch = std::mem::replace(&mut self.diags, saved);
            let _ = scratch;
            if let Some(EnvValue::Secret(name)) = value {
                job_secret_env.push((key.clone(), name));
            }
        }
        let _ = matrix;

        // Matrix.
        let matrix_items = self.apply_strategy(job, &mut site);

        // The gate: needs + the job's own if — and, inside a called workflow,
        // the call's own admission.
        let gate = self.condition(job.condition, &site, false, job.span.clone());
        let gate = self.wrap_call_admission(gate, &site);
        if let Some(gate) = gate {
            self.b.set_precondition(start, gate);
        }

        // Steps.
        let mut names_so_far: Vec<String> = Vec::new();
        let mut step_names: BTreeMap<String, String> = BTreeMap::new();
        for step in &job.steps {
            step_names.insert(
                step.node_name(),
                format!("{}{SEP}{}", job.id, step.node_name()),
            );
        }
        site.step_names = step_names;

        // A JavaScript action with `pre` or `post` contributes nodes away from its
        // own position: GitHub runs every `pre` before the first step and every
        // `post` after the last, in reverse order. Resolved once, quietly; the main
        // pass reports whatever is wrong with the reference.
        let plans: Vec<Option<ActionPlan>> = job
            .steps
            .iter()
            .map(|step| self.action_plan(step))
            .collect();

        let mut previous = start;
        let mut chain: Vec<NodeId> = Vec::new();
        for (step, plan) in job.steps.iter().zip(&plans) {
            if let Some(plan) = plan
                && plan.node.pre.is_some()
                && let Some(id) = self.lifecycle_node(
                    ActionContext {
                        job,
                        step,
                        scope,
                        site: &site,
                        job_secret_env: &job_secret_env,
                    },
                    plan,
                    Phase::Pre,
                )
            {
                self.chain_node(&mut previous, &mut chain, &mut names_so_far, &mut site, id);
            }
        }
        for (step, plan) in job.steps.iter().zip(&plans) {
            let inherited = Defaults {
                shell: step.shell.or(job.defaults.shell).or(self.wf.defaults.shell),
                working_directory: step
                    .working_directory
                    .or(job.defaults.working_directory)
                    .or(self.wf.defaults.working_directory),
            };
            let nodes = self.step_nodes(
                job,
                step,
                scope,
                &mut site,
                &names_so_far,
                &job_secret_env,
                inherited,
                0,
                plan.as_ref(),
            );
            for id in nodes {
                self.chain_node(&mut previous, &mut chain, &mut names_so_far, &mut site, id);
            }
        }
        for (step, plan) in job.steps.iter().zip(&plans).rev() {
            if let Some(plan) = plan
                && plan.node.post.is_some()
                && let Some(id) = self.lifecycle_node(
                    ActionContext {
                        job,
                        step,
                        scope,
                        site: &site,
                        job_secret_env: &job_secret_env,
                    },
                    plan,
                    Phase::Post,
                )
            {
                self.chain_node(&mut previous, &mut chain, &mut names_so_far, &mut site, id);
            }
        }

        let last = *chain.last().unwrap_or(&start);
        if let Some(j) = self.jobs.get_mut(&job.id) {
            j.last = last;
        }

        // The last step's edge carries the job summary into `done`. It is evaluated in
        // the last step's outcome context, so `status` is that step's own.
        let summary = self.job_summary(job, &site, last == start);
        let ids = self
            .b
            .select(last, vec![ir::Arm::always(done).with_map(summary)]);
        let _ = ids;

        // `done` folds its inputs — one for a plain job, one per leg for a matrix.
        let fold = self.done_config();
        self.b.node_mut(done).step = StepRef::new("noop", fold);

        if let Some(items) = matrix_items {
            let target = if last == start {
                ExpandTarget::Node
            } else {
                ExpandTarget::Subgraph {
                    entry: start,
                    exit: last,
                }
            };
            ir::parallel_for_each(
                &mut self.b,
                start,
                items,
                target,
                site.max_parallel,
                site.fail_fast,
            );
        }
    }

    /// `done` → each dependent's `start`.
    /// `done` → each dependent's `start`, within the entry's own frame — and,
    /// inside a called workflow, → the call's exit join, which counts every
    /// callee job.
    fn job_edges(&mut self, e: &Entry<'a>, entries: &[Entry<'a>]) {
        let job = &e.job;
        let Some(done) = self.jobs.get(&job.id).map(|j| j.done) else {
            return;
        };
        let mut targets: Vec<NodeId> = entries
            .iter()
            .filter(|other| other.frame == e.frame)
            .filter(|other| other.job.needs.iter().any(|(n, _)| n == &job.id))
            .filter_map(|other| self.jobs.get(&other.job.id).map(|j| j.start))
            .collect();
        for (need, span) in &job.needs {
            if !self.jobs.contains_key(need) {
                self.diags.error(
                    "gha.needs_unknown",
                    span.clone(),
                    format!("job `{}` needs unknown job `{need}`", job.id),
                );
            }
        }
        if let Some(exit) = self.frame_ctx[e.frame].exit {
            targets.push(exit);
        }
        if !targets.is_empty() {
            self.b.fan_out(done, &targets);
        }
    }

    // ── Steps ──────────────────────────────────────────────────────────────

    /// Link `id` after `previous` and make it visible to the steps after it.
    /// Every chained node runs on cancel: after a polite cancel it fires and its
    /// gate decides, which is the whole cancellation story (no admission
    /// sniffing). Matrix templates pass the flag to their clones.
    fn chain_node(
        &mut self,
        previous: &mut NodeId,
        chain: &mut Vec<NodeId>,
        names: &mut Vec<String>,
        site: &mut Site,
        id: NodeId,
    ) {
        self.b.node_mut(id).run_on_cancel = true;
        self.b.link(*previous, id);
        chain.push(id);
        *previous = id;
        let name = self
            .b
            .graph()
            .node(id)
            .map(|n| n.name.to_string())
            .unwrap_or_default();
        names.push(name);
        site.earlier_steps = names.clone();
    }

    /// The node(s) for one step: one `github/run` node, one `github/action` node,
    /// or a composite's inlined chain.
    #[allow(clippy::too_many_arguments)]
    fn step_nodes(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        scope: ScopeId,
        site: &mut Site,
        earlier: &[String],
        job_secret_env: &[(String, String)],
        inherited: Defaults<'_>,
        depth: usize,
        action_plan: Option<&ActionPlan>,
    ) -> Vec<NodeId> {
        let node_name = match site.action_inputs.is_some() {
            true => format!("{}{SEP}{}", site.job_id, step.node_name()),
            false => format!("{}{SEP}{}", job.id, step.node_name()),
        };

        if let Some((reference, span)) = &step.uses {
            return self.uses_step(
                job,
                step,
                reference,
                span,
                scope,
                site,
                earlier,
                job_secret_env,
                depth,
                action_plan,
            );
        }

        let mut step_site = site.clone();
        step_site.earlier_steps = earlier.to_vec();
        let env_config = self.step_env_config(step, &mut step_site, job_secret_env);

        let mut config = Map::new();
        let Some(run) = step.run else {
            return Vec::new();
        };
        let Some(run_text) = run.as_str() else {
            self.diags
                .error("gha.bad_step", run.span(), "`run` must be a string");
            return Vec::new();
        };
        match lower_scalar(
            run_text,
            run.span(),
            &step_site,
            true,
            true,
            self.b.exprs(),
            &mut self.diags,
        ) {
            Some(v) => {
                config.insert("run".into(), config_value(v));
            }
            None => return Vec::new(),
        }

        // Shell. `bash` and `sh` invoke directly; `python` and `pwsh` get GitHub's
        // documented template; any custom template with a `{0}` is taken as
        // written — the step writes the script to a file and substitutes its path,
        // as GitHub does. Windows-only shells stay rejected.
        let shell = match inherited.shell {
            None => "bash",
            Some(node) => node.as_str().unwrap_or(""),
        };
        match shell {
            "bash" => {
                config.insert("shell".into(), json!("bash"));
            }
            "sh" => {
                config.insert("shell".into(), json!("sh"));
            }
            "python" => {
                config.insert("shell_command".into(), json!("python {0}"));
            }
            "pwsh" => {
                config.insert("shell_command".into(), json!("pwsh -command \". '{0}'\""));
            }
            other if other.contains("{0}") && !other.contains("${{") => {
                config.insert("shell_command".into(), json!(other));
            }
            other => {
                let kind = other.split_whitespace().next().unwrap_or(other);
                self.diags.unsupported(
                    &format!(
                        "shell.{}",
                        kind.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                    ),
                    inherited.shell.map(|n| n.span()).unwrap_or_default(),
                    format!("`shell: {other}`"),
                    "`bash`, `sh`, `python`, `pwsh` and custom `{0}` templates are available; \
                     Windows shells are not",
                );
            }
        }

        if let Some(wd) = inherited.working_directory
            && let Some(text) = wd.as_str()
            && let Some(v) = lower_scalar(
                text,
                wd.span(),
                &step_site,
                true,
                false,
                self.b.exprs(),
                &mut self.diags,
            )
        {
            config.insert("working_dir".into(), config_value(v));
        }

        // continue-on-error → soft_fail.
        if let Some(coe) = step.continue_on_error {
            match coe.as_scalar().and_then(|s| s.as_bool()) {
                Some(true) => {
                    config.insert("soft_fail".into(), json!(true));
                }
                Some(false) => {}
                None => self.diags.unsupported(
                    "continue_on_error.expression",
                    coe.span(),
                    "an expression-valued `continue-on-error`",
                    "use a literal true or false",
                ),
            }
        }
        if !env_config.is_empty() {
            config.insert("env".into(), Value::Object(env_config));
        }
        config.insert("event".into(), self.event_config());

        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(RUN_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        self.gate_main_node(id, step, &step_site);
        vec![id]
    }

    /// The step's `env:` as config, secrets from the job pushed down first. Step env
    /// is also made visible to the step's own expressions through `site`.
    fn step_env_config(
        &mut self,
        step: &Step<'_>,
        step_site: &mut Site,
        job_secret_env: &[(String, String)],
    ) -> Map<String, Value> {
        let mut env_config = Map::new();
        for (key, name) in job_secret_env {
            env_config.insert(
                key.clone(),
                json!({ ir::placeholder::SECRET_REF_KEY: name }),
            );
        }
        for (key, node) in &step.env {
            match self.env_value(node, step_site, true) {
                Some(EnvValue::Plain(v)) => {
                    step_site.step_env.insert(key.clone(), v.clone());
                    env_config.insert(
                        key.clone(),
                        match v {
                            ExprOrValue::Value(v) => v,
                            ExprOrValue::Expr(id) => json!({ EXPR_PLACEHOLDER_KEY: id.raw() }),
                        },
                    );
                }
                Some(EnvValue::Secret(name)) => {
                    env_config.insert(
                        key.clone(),
                        json!({ ir::placeholder::SECRET_REF_KEY: name }),
                    );
                }
                None => {}
            }
        }
        env_config
    }

    /// Timeout: step, else job, else GitHub's default.
    fn set_step_budget(&mut self, id: NodeId, job: &Job<'a>, step: &Step<'_>) {
        let timeout = self
            .minutes(step.timeout_minutes)
            .or_else(|| self.minutes(job.timeout_minutes))
            .unwrap_or(DEFAULT_TIMEOUT);
        self.b.set_budget(id, Budget::new(1, timeout));
    }

    /// A main step's gate: the job started, and the step's own condition (default
    /// `success()` over earlier steps), attached to the node's config for the
    /// step kind to evaluate at spawn. The node carries no engine precondition.
    fn gate_main_node(&mut self, id: NodeId, step: &Step<'_>, step_site: &Site) {
        let started = step_site.job_started(self.b.exprs());
        let gate = self.step_gate(step.condition, step_site, step.span.clone(), &[started]);
        self.attach_gate(id, gate);
    }

    /// The step-level condition as a gate. `prereqs` are engine-side terms ANDed
    /// in front — the job-started term, a post node's main-ran term.
    ///
    /// A condition with nothing only the step can resolve — no `env.*`, no
    /// `hashFiles` — collapses with the prerequisites into a single engine
    /// expression: one `$expr` leaf, the common case. Otherwise the condition
    /// splits on its operators into a gate tree ([`gate::condition_tree`]).
    fn step_gate(
        &mut self,
        node: Option<Node<'_>>,
        site: &Site,
        span: Span,
        prereqs: &[ExprId],
    ) -> Value {
        if let Some(scalar) = node.and_then(|n| n.as_scalar())
            && scalar.as_bool().is_none()
            && let Ok(source) = if_expr_source(scalar.as_str())
        {
            return self.step_gate_text(&source, site, span, prereqs);
        }
        let cond = self.condition(node, site, true, span);
        self.collapse_gate(prereqs, cond)
    }

    /// [`Self::step_gate`] over bare condition text (a `pre-if`, a `post-if`).
    fn step_gate_text(
        &mut self,
        source: &str,
        site: &Site,
        span: Span,
        prereqs: &[ExprId],
    ) -> Value {
        if let Ok(ast) = parse(source)
            && gate::needs_lazy(&ast)
        {
            return self.lazy_gate(&ast, site, span, prereqs);
        }
        let cond = self.condition_text(source, site, true, span);
        self.collapse_gate(prereqs, cond)
    }

    /// The all-engine case: one `$expr` leaf holding `prereq && … && condition`.
    fn collapse_gate(&mut self, prereqs: &[ExprId], cond: Option<ExprId>) -> Value {
        let mut acc: Option<ExprId> = None;
        for term in prereqs.iter().copied().chain(cond) {
            acc = Some(match acc {
                None => term,
                Some(a) => self.b.exprs().binary(BinOp::And, a, term),
            });
        }
        let id = acc.unwrap_or_else(|| self.b.exprs().lit(true));
        Gate::expr(id).to_value()
    }

    /// The gate tree for a condition with step-resolved leaves: prerequisites and
    /// the implicit `success()` (unless the condition names a status function) as
    /// engine leaves, then the condition split on its operators. GitHub
    /// truthiness lands at the root, in the step's evaluator.
    fn lazy_gate(
        &mut self,
        ast: &frontend::expr::Expr,
        site: &Site,
        span: Span,
        prereqs: &[ExprId],
    ) -> Value {
        let mut terms: Vec<Gate> = prereqs.iter().map(|id| Gate::expr(*id)).collect();
        if !names_status_function(ast) {
            let success = site.status_function(self.b.exprs(), "success", true);
            terms.push(Gate::expr(success));
        }
        if let Some(tree) = gate::condition_tree(ast, site, &span, self.b.exprs(), &mut self.diags)
        {
            terms.push(tree);
        }
        let gate = match terms.len() {
            1 => terms.pop().expect("one term"),
            _ => Gate::Op {
                op: GateOp::And,
                args: terms,
            },
        };
        gate.to_value()
    }

    /// The engine's `scope_cancelled` static as a config placeholder: the bit that
    /// turns a false gate into a `Cancelled` record rather than a `Skipped` one,
    /// mirroring how the engine records a false precondition in a cancelled scope.
    fn scope_cancelled_config(&mut self) -> Value {
        let id = self.b.exprs().var("scope_cancelled");
        json!({ EXPR_PLACEHOLDER_KEY: id.raw() })
    }

    /// Put the gate (and the cancelled bit) into a step node's config.
    fn attach_gate(&mut self, id: NodeId, gate: Value) {
        let cancelled = self.scope_cancelled_config();
        let node = self.b.node_mut(id);
        if let Value::Object(map) = &mut node.step.config {
            map.insert("gate".into(), gate);
            map.insert("cancelled".into(), cancelled);
        }
    }

    /// AND a composite caller's gate in front of an inlined node's own.
    fn wrap_gate(&mut self, id: NodeId, caller: &Value) {
        let own = match self.b.graph().node(id).map(|n| &n.step.config) {
            Some(Value::Object(map)) => map.get("gate").cloned(),
            _ => None,
        };
        let Some(own) = own else { return };
        let wrapped = json!({
            gate::OP_KEY: GateOp::And.symbol(),
            gate::ARGS_KEY: [caller.clone(), own],
        });
        let node = self.b.node_mut(id);
        if let Value::Object(map) = &mut node.step.config {
            map.insert("gate".into(), wrapped);
        }
    }

    /// `github.event`, for `GITHUB_EVENT_PATH`.
    fn event_config(&mut self) -> Value {
        let t = self.b.exprs();
        let github = t.var("github");
        let key = t.lit("event");
        let event = t.call("get_ci", vec![github, key]);
        json!({ EXPR_PLACEHOLDER_KEY: event.raw() })
    }

    /// The state an earlier phase of the same action saved, read from its record.
    fn state_config(&mut self, site: &Site, from: &str) -> Value {
        let t = self.b.exprs();
        let record = site.node_record(t, from);
        let output = t.field(record, "output");
        let key = t.lit(STATE_OUTPUT_KEY);
        let state = t.index(output, key);
        let empty = t.object(vec![]);
        let state = t.call("default", vec![state, empty]);
        json!({ EXPR_PLACEHOLDER_KEY: state.raw() })
    }

    /// `true` when the action's main node has a record other than `skipped`: the
    /// condition for its `post` to run at all.
    fn main_ran(&mut self, site: &Site, main: &str) -> ExprId {
        let t = self.b.exprs();
        let status = site.node_status(t, main);
        let skipped = t.lit("skipped");
        let status = t.call("default", vec![status, skipped]);
        let is_skipped = t.binary(BinOp::Eq, status, skipped);
        t.unary(UnOp::Not, is_skipped)
    }

    // ── `uses:` ────────────────────────────────────────────────────────────

    /// Resolve `owner/repo@ref` to its commit and manifest, once per reference.
    fn resolve_remote(&mut self, name: &str) -> Result<(PinnedAction, String), ResolveFailure> {
        if let Some(cached) = self.resolved.get(name) {
            return cached.clone();
        }
        let source_failure = |e: ActionSourceError| match e {
            ActionSourceError::Unavailable { reason, .. } => ResolveFailure::Unavailable(reason),
            other => ResolveFailure::Failed(other.to_string()),
        };
        let result = match self.actions {
            None => Err(ResolveFailure::NoSource),
            Some(source) => ActionRef::parse(name)
                .map_err(|e| ResolveFailure::Failed(e.to_string()))
                .and_then(|reference| source.resolve(&reference).map_err(source_failure))
                .and_then(|pinned| {
                    pinned
                        .validate()
                        .map_err(|e| ResolveFailure::Failed(e.to_string()))?;
                    source
                        .manifest(&pinned)
                        .map(|text| (pinned, text))
                        .map_err(source_failure)
                }),
        };
        self.resolved.insert(name.to_string(), result.clone());
        result
    }

    /// The `action.yml` document for a `uses:` reference, or the diagnostic saying
    /// why there is none.
    fn action_document(&mut self, reference: &str, span: &Span) -> Option<Document> {
        match composite::classify(reference) {
            Uses::Local(path) => {
                // Inside a workflow fetched from another repository, `./` names a
                // file of that repository — which the lowering does not stage.
                if self.frame_ctx[self.current].remote {
                    self.diags.unsupported(
                        "action.nested_local",
                        span.clone(),
                        format!("`uses: ./{path}` inside a remote called workflow"),
                        "a relative action inside a fetched workflow resolves against that \
                         repository, which the lowering does not stage",
                    );
                    return None;
                }
                composite::read_document(self.files, &path, span, &mut self.diags)
            }
            Uses::Docker(image) => {
                self.diags.unsupported(
                    "action.docker",
                    span.clone(),
                    format!("`uses: docker://{image}`"),
                    "Docker actions run against the daemon, not through the job environment; not yet built",
                );
                None
            }
            Uses::Remote(name) => match self.resolve_remote(&name) {
                Ok((pinned, text)) => Document::parse(
                    &format!("{}/action.yml", pinned.reference),
                    &text,
                    &mut self.diags,
                ),
                Err(ResolveFailure::NoSource) => {
                    self.diags.unsupported(
                        "action.remote",
                        span.clone(),
                        name.to_string(),
                        "no action source is configured, so actions from other repositories cannot be fetched",
                    );
                    None
                }
                Err(ResolveFailure::Unavailable(reason)) => {
                    // Two different situations, one code: with no reason the
                    // source simply does not cover the reference (refreshing it
                    // may help); with one, upstream already said no (a private or
                    // removed repository — refreshing will not).
                    let hint = match reason {
                        Some(reason) => format!(
                            "the action source cannot serve it: {} — the repository is unavailable \
                             upstream (private or removed), so refreshing the source will not help",
                            reason.lines().collect::<Vec<_>>().join(" ")
                        ),
                        None => "the configured action source does not serve this reference; \
                                 refreshing it (for a snapshot, the refresh test) may add it"
                            .to_string(),
                    };
                    self.diags
                        .unsupported("action.remote", span.clone(), name.to_string(), &hint);
                    None
                }
                Err(ResolveFailure::Failed(message)) => {
                    self.diags.error(
                        "action.unresolved",
                        span.clone(),
                        format!("`uses: {name}`: {message}"),
                    );
                    None
                }
            },
        }
    }

    /// The plan for a `uses:` step that is a JavaScript action; `None` for anything
    /// else, quietly — the main pass reports problems.
    fn action_plan(&mut self, step: &Step<'_>) -> Option<ActionPlan> {
        let (reference, span) = step.uses.as_ref()?;
        let saved = std::mem::replace(&mut self.diags, Diagnostics::new());
        let plan = self.action_plan_inner(reference, span);
        let plan_diags = std::mem::replace(&mut self.diags, saved);
        if plan.is_some() {
            self.diags.extend(plan_diags);
        }
        plan
    }

    fn action_plan_inner(&mut self, reference: &str, span: &Span) -> Option<ActionPlan> {
        let location = self.location_of(reference)?;
        let doc = self.action_document(reference, span)?;
        let manifest = composite::read_manifest(&doc, &mut self.diags)?;
        let Runs::Node(node) = manifest.runs else {
            return None;
        };
        Some(ActionPlan {
            location,
            node,
            inputs: plan_inputs(&manifest.inputs),
        })
    }

    /// Where a `uses:` reference's files are found at run time; `None` when it
    /// names no fetched or local tree.
    fn location_of(&mut self, reference: &str) -> Option<ActionLocation> {
        match composite::classify(reference) {
            Uses::Local(path) => Some(ActionLocation::Local { local: path }),
            Uses::Docker(_) => None,
            Uses::Remote(name) => Some(ActionLocation::Pinned(self.resolve_remote(&name).ok()?.0)),
        }
    }

    /// A `pre` or `post` node for a JavaScript action. `pre-if` and `post-if`
    /// default to `always()`; a `post` also needs its main node to have run.
    fn lifecycle_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        phase: Phase,
    ) -> Option<NodeId> {
        let ActionContext { step, site, .. } = context;
        let (_, span) = step.uses.as_ref()?;
        let source = match phase {
            Phase::Pre => plan.node.pre_if.as_deref(),
            Phase::Post => plan.node.post_if.as_deref(),
            Phase::Main => return None,
        }
        .unwrap_or("always()");
        let main_name = format!("{}{SEP}{}", site.job_id, step.node_name());
        let state_from = (phase == Phase::Post).then(|| main_name.clone());
        let id = self.action_node(context, plan, phase, state_from.as_deref());
        let mut prereqs = vec![site.job_started(self.b.exprs())];
        if phase == Phase::Post {
            prereqs.push(self.main_ran(site, &main_name));
        }
        let gate = self.step_gate_text(source, site, span.clone(), &prereqs);
        self.attach_gate(id, gate);
        Some(id)
    }

    /// One `github/action` node: the action pinned, its phase and entry point, its
    /// inputs and env lowered where the step is.
    fn action_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        phase: Phase,
        state_from: Option<&str>,
    ) -> NodeId {
        let ActionContext {
            job,
            step,
            scope,
            site,
            job_secret_env,
        } = context;
        let (uses, span) = step
            .uses
            .as_ref()
            .map(|(u, s)| (u.as_str(), s.clone()))
            .unwrap_or_default();
        let entry = match phase {
            Phase::Pre => plan.node.pre.clone(),
            Phase::Main => Some(plan.node.main.clone()),
            Phase::Post => plan.node.post.clone(),
        }
        .unwrap_or_default();
        let suffix = match phase {
            Phase::Pre => format!("{SEP}pre"),
            Phase::Main => String::new(),
            Phase::Post => format!("{SEP}post"),
        };
        let node_name = format!("{}{SEP}{}{suffix}", site.job_id, step.node_name());

        let mut step_site = site.clone();
        let env_config = self.step_env_config(step, &mut step_site, job_secret_env);

        // Inputs: declared ones take the caller's `with:`, else their default;
        // undeclared `with:` keys pass through as GitHub does (with a warning there).
        let mut with: BTreeMap<String, Node<'_>> = BTreeMap::new();
        for (k, v) in &step.with {
            with.insert(k.to_lowercase(), *v);
        }
        let mut inputs = Map::new();
        let mut declared: HashSet<String> = HashSet::new();
        for input in &plan.inputs {
            let key = input.name.to_lowercase();
            declared.insert(key.clone());
            let value = match with.get(&key) {
                Some(node) => self.with_value(*node, &step_site),
                None => match &input.default {
                    Some(text) => self.text_value(text, span.clone(), &step_site),
                    None => {
                        if input.required && phase == Phase::Main {
                            self.diags.error(
                                "gha.missing_input",
                                span.clone(),
                                format!("`{uses}` requires input `{}`", input.name),
                            );
                        }
                        None
                    }
                },
            };
            if let Some(value) = value {
                inputs.insert(input.name.clone(), value);
            }
        }
        for (key, node) in &step.with {
            if !declared.contains(&key.to_lowercase())
                && let Some(value) = self.with_value(*node, &step_site)
            {
                inputs.insert(key.clone(), value);
            }
        }

        let mut config = Map::new();
        config.insert(
            "action".into(),
            serde_json::to_value(&plan.location).expect("an action location serializes"),
        );
        config.insert("entry".into(), json!(entry));
        config.insert("inputs".into(), Value::Object(inputs));
        if !env_config.is_empty() {
            config.insert("env".into(), Value::Object(env_config));
        }
        config.insert("event".into(), self.event_config());
        if let Some(from) = state_from {
            config.insert("state".into(), self.state_config(site, from));
        }
        if let Some(coe) = step.continue_on_error {
            match coe.as_scalar().and_then(|s| s.as_bool()) {
                Some(true) => {
                    config.insert("soft_fail".into(), json!(true));
                }
                Some(false) => {}
                None if phase == Phase::Main => self.diags.unsupported(
                    "continue_on_error.expression",
                    coe.span(),
                    "an expression-valued `continue-on-error`",
                    "use a literal true or false",
                ),
                None => {}
            }
        }

        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(ACTION_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        id
    }

    fn main_action_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        earlier: &[String],
    ) -> Vec<NodeId> {
        let mut step_site = context.site.clone();
        step_site.earlier_steps = earlier.to_vec();
        let main_name = format!("{}{SEP}{}", context.site.job_id, context.step.node_name());
        let state_from = (context.site.action_inputs.is_none() && plan.node.pre.is_some())
            .then(|| format!("{main_name}{SEP}pre"));
        let main_context = ActionContext {
            site: &step_site,
            ..context
        };
        let id = self.action_node(main_context, plan, Phase::Main, state_from.as_deref());
        self.gate_main_node(id, context.step, &step_site);
        vec![id]
    }

    /// A `with:` value: a string, possibly templated; anything else stringified.
    fn with_value(&mut self, node: Node<'_>, site: &Site) -> Option<Value> {
        match node.as_str() {
            Some(text) => self.text_value(text, node.span(), site),
            None => Some(Value::String(scalar_text(node))),
        }
    }

    /// Text from the workflow or an action manifest as an input value: literal,
    /// expression, or a whole-value secret.
    fn text_value(&mut self, text: &str, span: Span, site: &Site) -> Option<Value> {
        lower_scalar(
            text,
            span,
            site,
            true,
            true,
            self.b.exprs(),
            &mut self.diags,
        )
        .map(config_value)
    }

    /// `uses:` — a composite (local or remote) is inlined; a JavaScript action
    /// becomes a `github/action` node; a Docker action is rejected.
    #[allow(clippy::too_many_arguments)]
    fn uses_step(
        &mut self,
        job: &Job<'a>,
        step: &Step<'_>,
        reference: &str,
        span: &Span,
        scope: ScopeId,
        site: &mut Site,
        earlier: &[String],
        job_secret_env: &[(String, String)],
        depth: usize,
        action_plan: Option<&ActionPlan>,
    ) -> Vec<NodeId> {
        if depth >= composite::MAX_DEPTH {
            self.diags.error(
                "gha.composite_depth",
                span.clone(),
                format!(
                    "composite actions nest more than {} deep at `{reference}`",
                    composite::MAX_DEPTH
                ),
            );
            return Vec::new();
        }
        if let Some(plan) = action_plan {
            return self.main_action_node(
                ActionContext {
                    job,
                    step,
                    scope,
                    site,
                    job_secret_env,
                },
                plan,
                earlier,
            );
        }
        let Some(doc) = self.action_document(reference, span) else {
            return Vec::new();
        };
        let Some(manifest) = composite::read_manifest(&doc, &mut self.diags) else {
            return Vec::new();
        };
        let action = match manifest.runs {
            Runs::Composite(action) => action,
            Runs::Node(node) => {
                // The manifest in hand is the plan; no need to read it again.
                let Some(location) = self.location_of(reference) else {
                    return Vec::new();
                };
                let plan = ActionPlan {
                    location,
                    node,
                    inputs: plan_inputs(&manifest.inputs),
                };
                if depth > 0 && (plan.node.pre.is_some() || plan.node.post.is_some()) {
                    self.diags.warning(
                        "action.nested_lifecycle",
                        span.clone(),
                        format!(
                            "`{reference}` has `pre` or `post` steps, which do not run inside a composite action here; its main step does"
                        ),
                    );
                }
                return self.main_action_node(
                    ActionContext {
                        job,
                        step,
                        scope,
                        site,
                        job_secret_env,
                    },
                    &plan,
                    earlier,
                );
            }
            Runs::Docker => {
                self.diags.unsupported(
                    "action.docker",
                    span.clone(),
                    format!("`{reference}` is a Docker container action"),
                    "Docker actions run against the daemon, not through the job environment; not yet built",
                );
                return Vec::new();
            }
        };
        if matches!(composite::classify(reference), Uses::Remote(_))
            && action
                .steps
                .iter()
                .any(|s| matches!(&s.uses, Some((u, _)) if u.starts_with('.')))
        {
            self.diags.unsupported(
                "action.nested_local",
                span.clone(),
                format!("`{reference}` uses a `./` action inside a remote composite"),
                "a relative action inside a fetched composite resolves against that repository, which the lowering does not stage",
            );
            return Vec::new();
        }

        // Inputs: the caller's `with`, lowered in the caller's site, else the default.
        let caller_site = {
            let mut s = site.clone();
            s.earlier_steps = earlier.to_vec();
            s
        };
        let mut inputs: BTreeMap<String, ExprId> = BTreeMap::new();
        let mut with: BTreeMap<String, Node<'_>> = BTreeMap::new();
        for (k, v) in &step.with {
            with.insert(k.to_lowercase(), *v);
        }
        for input in &action.inputs {
            let node = with
                .get(&input.name.to_lowercase())
                .copied()
                .or(input.default);
            match node {
                Some(n) => {
                    let text = n.as_str().unwrap_or("");
                    match lower_scalar(
                        text,
                        n.span(),
                        &caller_site,
                        true,
                        true,
                        self.b.exprs(),
                        &mut self.diags,
                    ) {
                        Some(LoweredScalar::Literal(v)) => {
                            let id = self.b.exprs().lit(v);
                            inputs.insert(input.name.clone(), id);
                        }
                        Some(LoweredScalar::Expr(id)) => {
                            inputs.insert(input.name.clone(), id);
                        }
                        Some(LoweredScalar::Secret(name)) => {
                            // Into the inner steps' expressions it goes as its
                            // sentinel; the step that receives it resolves it at spawn.
                            let id = self.b.exprs().lit(secret_sentinel(&name));
                            inputs.insert(input.name.clone(), id);
                        }
                        None => {}
                    }
                }
                None if input.required => {
                    self.diags.error(
                        "gha.missing_input",
                        span.clone(),
                        format!("`{reference}` requires input `{}`", input.name),
                    );
                }
                None => {
                    let id = self.b.exprs().lit(Value::Null);
                    inputs.insert(input.name.clone(), id);
                }
            }
        }

        // The composite's steps live under `job/caller/inner`.
        let caller = step.node_name();
        let mut inner_site = site.clone();
        inner_site.job_id = format!("{}{SEP}{caller}", site.job_id);
        inner_site.action_inputs = Some(inputs);
        inner_site.step_names = action
            .steps
            .iter()
            .map(|s| {
                (
                    s.node_name(),
                    format!("{}{SEP}{}", inner_site.job_id, s.node_name()),
                )
            })
            .collect();
        inner_site.composite_outputs = BTreeMap::new();

        // Inner steps observe the composite's own steps, not the caller's: the
        // action is a unit, and its steps' implicit `success()` must not re-judge
        // work outside it. The caller's gate below is what speaks for that work.
        let mut names_so_far: Vec<String> = Vec::new();
        inner_site.earlier_steps = Vec::new();
        let mut ids = Vec::new();
        for inner in &action.steps {
            let inherited = Defaults {
                shell: inner.shell,
                working_directory: inner.working_directory,
            };
            let nodes = self.step_nodes(
                job,
                inner,
                scope,
                &mut inner_site,
                &names_so_far,
                job_secret_env,
                inherited,
                depth + 1,
                None,
            );
            for id in nodes {
                let name = self
                    .b
                    .graph()
                    .node(id)
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                names_so_far.push(name);
                inner_site.earlier_steps = names_so_far.clone();
                ids.push(id);
            }
        }

        // The caller's `if:` gates the whole inlined unit the way a job's gate
        // does: there is no wrapper node, so it is ANDed into every inlined
        // node's gate (default: `success()` over the caller's earlier steps).
        // The caller's site is where the cancel-interrupt term lives, so a
        // composite admitted by `always()` after a cancel runs its inner steps
        // normally, and an un-gated one shuts them all off — no flags, no
        // condition-text sniffing.
        let caller_gate = self.step_gate(step.condition, &caller_site, span.clone(), &[]);
        for id in &ids {
            self.wrap_gate(*id, &caller_gate);
        }

        // Outputs: expressions over the inner steps, visible to the caller as
        // `steps.<caller>.outputs.<name>`.
        let mut outputs: BTreeMap<String, ExprId> = BTreeMap::new();
        for (name, value) in &action.outputs {
            let text = value.as_str().unwrap_or("");
            if let Some(lowered) = lower_scalar(
                text,
                value.span(),
                &inner_site,
                true,
                false,
                self.b.exprs(),
                &mut self.diags,
            ) {
                let id = match lowered {
                    LoweredScalar::Literal(v) => self.b.exprs().lit(v),
                    LoweredScalar::Expr(id) => id,
                    LoweredScalar::Secret(_) => continue,
                };
                outputs.insert(name.clone(), id);
            }
        }
        site.composite_outputs.insert(caller, outputs);
        ids
    }

    // ── Pieces ─────────────────────────────────────────────────────────────

    /// The `{ result, outputs }` summary the last step's edge carries into `done`,
    /// evaluated in that step's outcome context.
    fn job_summary(&mut self, job: &Job<'a>, site: &Site, no_steps: bool) -> ExprId {
        let t = self.b.exprs();
        // Own status counts too: `status` here is the last step's.
        let own = t.var("status");
        let own_failed = {
            let f = t.lit("failure");
            let to = t.lit("timed_out");
            let a = t.binary(BinOp::Eq, own, f);
            let b = t.binary(BinOp::Eq, own, to);
            t.binary(BinOp::Or, a, b)
        };
        let own_cancelled = {
            let c = t.lit("cancelled");
            t.binary(BinOp::Eq, own, c)
        };
        let earlier_failed = site.earlier_step_failed(t);
        let earlier_cancelled = site.earlier_step_cancelled(t);
        let failed = if no_steps {
            earlier_failed
        } else {
            t.binary(BinOp::Or, earlier_failed, own_failed)
        };
        let cancelled = if no_steps {
            earlier_cancelled
        } else {
            t.binary(BinOp::Or, earlier_cancelled, own_cancelled)
        };
        let started = site.job_started(t);
        let start_cancelled = site.start_cancelled(t);

        let s_failure = t.lit("failure");
        let s_cancelled = t.lit("cancelled");
        let s_success = t.lit("success");
        let s_skipped = t.lit("skipped");
        let inner = t.cond(cancelled, s_cancelled, s_success);
        let ran = t.cond(failed, s_failure, inner);
        // A job that never began: cancelled before its start could fire, else
        // skipped by its own gate — the distinction GitHub reports.
        let never_ran = t.cond(start_cancelled, s_cancelled, s_skipped);
        let result = t.cond(started, ran, never_ran);

        // Job outputs, lowered where every step is visible. An output that would
        // carry a secret is dropped with a warning, as GitHub drops it.
        let mut outputs: Vec<(String, ExprId)> = Vec::new();
        for (name, node) in &job.outputs {
            let text = node.as_str().unwrap_or("");
            if whole_value_secret(text).is_some() {
                self.diags.warning(
                    "ignored.secret_output",
                    node.span(),
                    format!(
                        "job output `{name}` would carry a secret; GitHub drops such an output, and so does this run"
                    ),
                );
                continue;
            }
            if let Some(lowered) = lower_scalar(
                text,
                node.span(),
                site,
                true,
                false,
                self.b.exprs(),
                &mut self.diags,
            ) {
                let id = match lowered {
                    LoweredScalar::Literal(v) => self.b.exprs().lit(v),
                    LoweredScalar::Expr(id) => id,
                    // Whole-value secrets were dropped above; `lower_scalar`
                    // rejects an embedded one before returning this.
                    LoweredScalar::Secret(_) => continue,
                };
                outputs.push((name.clone(), id));
            }
        }
        let t = self.b.exprs();
        let outputs_obj = t.object(outputs.iter().map(|(k, v)| (k.as_str(), *v)).collect());
        let index = if site.matrix {
            t.var("index")
        } else {
            t.lit(0)
        };
        t.object(vec![
            ("result", result),
            ("outputs", outputs_obj),
            ("index", index),
        ])
    }

    /// `done`'s config: fold `inputs` (one summary per leg) into one.
    fn done_config(&mut self) -> Value {
        let t = self.b.exprs();
        let inputs = t.var("inputs");
        let result_key = t.lit("result");
        let results = t.call("pluck", vec![inputs, result_key]);
        let has = |t: &mut ir::ExprTable, tag: &str| {
            let l = t.lit(tag);
            t.call("contains", vec![results, l])
        };
        let any_failure = has(t, "failure");
        let any_cancelled = has(t, "cancelled");
        let any_success = has(t, "success");
        let f = t.lit("failure");
        let c = t.lit("cancelled");
        let s = t.lit("success");
        let k = t.lit("skipped");
        let inner2 = t.cond(any_success, s, k);
        let inner1 = t.cond(any_cancelled, c, inner2);
        let result = t.cond(any_failure, f, inner1);

        // Outputs from the last leg to report, by index.
        let index_key = t.lit("index");
        let sorted = t.call("sort_by_key", vec![inputs, index_key]);
        let outputs_key = t.lit("outputs");
        let all_outputs = t.call("pluck", vec![sorted, outputs_key]);
        let len = t.call("len", vec![all_outputs]);
        let one = t.lit(1);
        let last = t.binary(BinOp::Sub, len, one);
        let outputs = t.index(all_outputs, last);
        let empty = t.object(vec![]);
        let outputs = t.call("default", vec![outputs, empty]);

        let summary = t.object(vec![("result", result), ("outputs", outputs)]);
        json!({ EXPR_PLACEHOLDER_KEY: summary.raw() })
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

    fn minutes(&mut self, node: Option<Node<'_>>) -> Option<Duration> {
        let node = node?;
        let scalar = node.as_scalar()?;
        if let Some(n) = scalar
            .as_i64()
            .map(|i| i as f64)
            .or_else(|| scalar.as_f64())
            && n > 0.0
        {
            return Some(Duration::from_secs_f64(n * 60.0));
        }
        self.diags.unsupported(
            "timeout.expression",
            node.span(),
            format!("`timeout-minutes: {}`", scalar.as_str()),
            "use a literal number of minutes",
        );
        None
    }

    /// `strategy.matrix` as an expression: a literal object with any `${{ }}` values
    /// lowered in place, or the whole thing an expression.
    fn matrix_expr(&mut self, node: Node<'_>, site: &Site) -> Option<ExprId> {
        self.yaml_expr(node, site)
    }

    fn yaml_expr(&mut self, node: Node<'_>, site: &Site) -> Option<ExprId> {
        if let Some(m) = node.as_mapping() {
            let mut fields = Vec::new();
            for (k, v) in m.iter() {
                let id = self.yaml_expr(v, site)?;
                fields.push((k.to_string(), id));
            }
            let t = self.b.exprs();
            return Some(t.object(fields.iter().map(|(k, v)| (k.as_str(), *v)).collect()));
        }
        if let Some(s) = node.as_sequence() {
            let mut items = Vec::new();
            for item in s.iter() {
                items.push(self.yaml_expr(item, site)?);
            }
            return Some(self.b.exprs().array(items));
        }
        let scalar = node.as_scalar()?;
        let text = scalar.as_str();
        if text.contains("${{") {
            return match lower_scalar(
                text,
                node.span(),
                site,
                false,
                false,
                self.b.exprs(),
                &mut self.diags,
            )? {
                LoweredScalar::Expr(id) => Some(id),
                LoweredScalar::Literal(v) => Some(self.b.exprs().lit(v)),
                LoweredScalar::Secret(_) => None,
            };
        }
        Some(self.b.exprs().lit(node.to_json()))
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

/// A manifest's declared inputs, owned, so the plan outlives the document.
fn plan_inputs(inputs: &[composite::Input<'_>]) -> Vec<PlanInput> {
    inputs
        .iter()
        .map(|input| PlanInput {
            name: input.name.clone(),
            default: input.default.and_then(scalar_text_opt),
            required: input.required,
        })
        .collect()
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
