//! Workflow graphs and their shared lowering passes. Reusable workflows
//! compile to registered child graphs; composite actions remain step
//! composition.

mod deferred;
mod jobs;
mod scope;
mod steps;
mod uses;
mod workflows;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

pub use deferred::{DeferredActionPlan, PlannedDeferredAction, plan_deferred_action};
use frontend::FileSource;
use frontend::diag::{Diagnostics, Lowered, Span};
use frontend::expr::lower::builtin;
use frontend::expr::{self, parse};
use frontend::yaml::Node;
use ir::{BinOp, ExprId, ExprOrValue, GraphBuilder, NodeId, ScopeId, Value};

use self::uses::ResolveFailure;
use self::workflows::Compiler;
use crate::action::{ActionLocation, ActionSource, Phase, PinnedAction};
use crate::call::{self, CalleeSource};
use crate::composite::{DockerAction, NodeAction};
use crate::exprs::{
    ExprSite, LoweredScalar, SEP, SecretMap, Site, lower_scalar, names_status_function,
};
use crate::model::{Job, Step, Workflow};
use crate::runners::RunnerMap;
use crate::{gate, runs_on};

type ActionResolutions = RefCell<HashMap<String, Result<PinnedAction, ResolveFailure>>>;

struct JobNodes {
    scope:  ScopeId,
    start:  NodeId,
    done:   NodeId,
    /// The last node of the step chain, which the expansion region ends at.
    last:   NodeId,
    matrix: bool,
}

pub(crate) struct Lowering<'w, 'a> {
    b:                   GraphBuilder,
    diags:               Diagnostics,
    wf:                  &'w Workflow<'a>,
    source:              CalleeSource,
    context:             WorkflowContext,
    is_invocation:       bool,
    files:               &'w dyn FileSource,
    /// Where `uses: owner/repo@ref` actions come from. `None` rejects them.
    actions:             Option<&'w dyn ActionSource>,
    /// Which `runs-on` labels place on this machine.
    runners:             &'w RunnerMap,
    /// Whether a supportable `actions/checkout` call becomes `github/checkout`
    /// (the local-checkout substitution) instead of running the real action.
    substitute_checkout: bool,
    /// Remote actions resolved so far, by reference as written: the pin, or
    /// why not. A reference used by several steps resolves once.
    resolved:            &'w ActionResolutions,
    jobs:                HashMap<String, JobNodes>,
    spans:               HashMap<NodeId, Span>,
    /// The job in flight's per-leg `runs-on` resolutions: what each matrix leg
    /// resolved to, written by [`Lowering::expression_runs_on`] and taken back
    /// within the same [`Lowering::job_shell`] call for the job's `start` node
    /// meta. Never live across jobs.
    leg_runs_on:         Option<Value>,
    /// The checkout's declared `github` identity — the repository slug from the
    /// origin remote where one exists — which placement guards evaluate
    /// against ([`crate::identity`]).
    github_identity:     Value,
    mode:                LoweringMode,
}

#[derive(Clone, Default)]
struct RuntimeSite {
    job_id:       String,
    start_node:   String,
    matrix:       bool,
    in_expansion: bool,
    needs:        BTreeMap<String, String>,
    depth:        usize,
    invocation:   bool,
}

enum LoweringMode {
    Deferred,
    Runtime {
        /// The one root action expanded eagerly. Actions found inside it remain
        /// deferred.
        eager_action: (String, String),
        /// Caller context restored while the runtime planner expands the root.
        site:         RuntimeSite,
    },
}

/// Input expressions and the static placement facts for this workflow.
#[derive(Default)]
struct WorkflowContext {
    inputs:        Option<BTreeMap<String, ExprId>>,
    static_inputs: BTreeMap<String, Value>,
    secrets:       SecretMap,
}

/// A `uses:` step that contributes standalone nodes — a JavaScript action or a
/// Docker container action — resolved once for everything it contributes:
/// `pre` and `post` are placed away from the main node.
struct ActionPlan {
    /// Where the action's files are. `None` for `uses: docker://…`, which has
    /// no files at all.
    location: Option<ActionLocation>,
    kind:     PlanKind,
    inputs:   Vec<PlanInput>,
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
    name:     String,
    /// The default's text, expressions and all; lowered where the step is.
    default:  Option<String>,
    required: bool,
}

#[derive(Clone, Copy)]
struct ActionContext<'s, 'job, 'step> {
    job:            &'s Job<'job>,
    step:           &'s Step<'step>,
    scope:          ScopeId,
    site:           &'s Site,
    job_secret_env: &'s [(String, String)],
}

pub(crate) fn lower(
    wf: &Workflow<'_>,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    runners: &RunnerMap,
    substitute_checkout: bool,
    diags: Diagnostics,
) -> Lowered {
    lower_internal(
        wf,
        files,
        actions,
        runners,
        substitute_checkout,
        diags,
        LoweringMode::Deferred,
    )
}

fn lower_internal(
    wf: &Workflow<'_>,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    runners: &RunnerMap,
    substitute_checkout: bool,
    mut diags: Diagnostics,
    mode: LoweringMode,
) -> Lowered {
    let calls = call::resolve(wf, files, actions, &mut diags);
    if diags.has_errors() {
        return Lowered::rejected(diags);
    }
    let models = calls.models();
    let resolved = ActionResolutions::default();
    let compiler = Compiler::new(
        &calls,
        &models,
        files,
        actions,
        runners,
        substitute_checkout,
        &resolved,
    );
    compiler.lower_root(wf, diags, mode)
}

impl<'a> Lowering<'_, 'a> {
    /// The one step the run-time planner expands eagerly — the root action of
    /// a deferred plan. `false` everywhere in a static lowering.
    fn is_eager_root(&self, job_id: &str, step_id: &str) -> bool {
        matches!(
            &self.mode,
            LoweringMode::Runtime {
                eager_action: (job, step),
                ..
            } if job == job_id && step == step_id
        )
    }

    fn runtime_site(&self) -> Option<&RuntimeSite> {
        match &self.mode {
            LoweringMode::Deferred => None,
            LoweringMode::Runtime { site, .. } => Some(site),
        }
    }

    /// The job's site, as its own `env:`, `if:` and `outputs:` see it: `needs`
    /// known, matrix-ness known, the workflow's inputs and secret map in scope,
    /// and no steps yet. `needs.*` keys name jobs in this workflow.
    fn base_site(&self, job: &Job<'a>) -> Site {
        let ctx = &self.context;
        let mut site = Site::new(&job.id);
        site.matrix = job.strategy.as_ref().is_some_and(|s| s.matrix.is_some());
        site.workflow_inputs.clone_from(&ctx.inputs);
        site.secrets = ctx.secrets.clone();
        site.invocation = self.is_invocation;
        if let Some(runtime) = self.runtime_site()
            && runtime.job_id == job.id
        {
            site.matrix = runtime.matrix;
            site.in_expansion = runtime.in_expansion;
            site.needs.clone_from(&runtime.needs);
            site.start_node.clone_from(&runtime.start_node);
        }
        for (need, _) in &job.needs {
            site.needs.insert(need.clone(), format!("{need}{SEP}done"));
        }
        site
    }

    /// The workflow's `inputs` as the placement resolver may read them: values
    /// known at lowering, with a marked placeholder for each run-time one so a
    /// label that absorbs it names its input instead of placing.
    pub(crate) fn placement_inputs(&self) -> Value {
        let ctx = &self.context;
        let mut inputs = serde_json::Map::new();
        if let Some(bound) = &ctx.inputs {
            for name in bound.keys() {
                let value = match ctx.static_inputs.get(name) {
                    Some(value) => value.clone(),
                    None => Value::String(runs_on::dynamic_placeholder(name)),
                };
                inputs.insert(name.clone(), value);
            }
        }
        Value::Object(inputs)
    }

    fn parameter(&mut self, name: &str) -> ExprId {
        Site::parameter(self.b.exprs(), name, self.is_invocation)
    }

    /// An `if:`: absent means `success()`; present is evaluated with GitHub's
    /// truthiness, and gets `success() &&` in front unless it names a status
    /// function.
    fn condition(
        &mut self,
        node: Option<Node<'_>>,
        site: &Site,
        at: ExprSite,
        span: Span,
    ) -> Option<ExprId> {
        let Some(node) = node else {
            return Some(site.status_function(self.b.exprs(), "success", at));
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
            let success = site.status_function(self.b.exprs(), "success", at);
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
        self.condition_text(&source, site, at, span)
    }

    /// One condition's expression text (the body of an `if:`, a `pre-if`, a
    /// `post-if`) as a precondition: GitHub's truthiness, with `success() &&`
    /// in front unless it names a status function.
    fn condition_text(
        &mut self,
        source: &str,
        site: &Site,
        at: ExprSite,
        span: Span,
    ) -> Option<ExprId> {
        let ast = match parse(source) {
            Ok(ast) => ast,
            Err(error) => {
                self.diags.error(
                    "expr.parse",
                    span,
                    format!("could not parse condition: {error}"),
                );
                return None;
            }
        };
        let uses_status_function = names_status_function(&ast);
        let expr = gate::condition_expr(&ast, site, at, &span, self.b.exprs(), &mut self.diags)?;
        let truthy = builtin(self.b.exprs(), "loose_truthy", vec![expr]).ok()?;
        if uses_status_function {
            Some(truthy)
        } else {
            let success = site.status_function(self.b.exprs(), "success", at);
            Some(self.b.exprs().binary(BinOp::And, success, truthy))
        }
    }

    fn env_value(&mut self, node: &Node<'_>, site: &Site, at: ExprSite) -> Option<EnvValue> {
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
            at,
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
}

enum EnvValue {
    Plain(ExprOrValue),
    Secret(String),
}

/// A YAML scalar as the string GitHub would pass: text as written, other
/// scalars stringified, null empty.
fn scalar_text(node: Node<'_>) -> String {
    scalar_text_opt(node).unwrap_or_default()
}

/// [`scalar_text`], with a YAML null as `None`: an input whose `default:` is
/// null (or missing a value) has no default, and GitHub leaves it unset rather
/// than passing `"null"` — while an explicit `default: ''` is the empty string.
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

/// The one expression an `if:` string holds: the text itself, or the body of
/// its single `${{ }}`. Both readers of an `if:` go through here, so they
/// cannot disagree on what counts as one expression.
fn if_expr_source(text: &str) -> Result<String, IfTemplateError> {
    if !text.contains("${{") {
        return Ok(text.to_string());
    }
    match expr::split_template(text) {
        Ok(segments) => match segments.as_slice() {
            [expr::Segment::Expr { source, .. }] => Ok(source.clone()),
            _ => Err(IfTemplateError::Mixed),
        },
        Err(_) => Err(IfTemplateError::Unterminated),
    }
}
