//! `uses:` steps: remote references resolved and pinned, JavaScript actions
//! planned and placed (`pre`, main, `post`), composites inlined, Docker
//! actions rejected.

use std::collections::{BTreeMap, HashSet};

use frontend::diag::{Diagnostics, Span};
use frontend::yaml::{Document, Node};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{BinOp, ExprId, NodeId, ScopeId, StepRef, UnOp, Value};
use serde_json::{Map, json};

use crate::action::{
    ACTION_KIND, ActionLocation, ActionRef, ActionSourceError, Phase, PinnedAction,
    STATE_OUTPUT_KEY, unavailable_hint,
};
use crate::composite::{self, Runs, Uses};
use crate::exprs::{LoweredScalar, SEP, Site, config_value, lower_scalar, secret_sentinel};
use crate::gate::{self, GateOp};
use crate::model::{Defaults, Job, Step};

use super::{ActionContext, ActionPlan, Lowering, PlanInput, scalar_text, scalar_text_opt};

/// Why a remote action did not resolve.
#[derive(Clone)]
pub(super) enum ResolveFailure {
    /// No [`ActionSource`] was given: the format is running without one.
    NoSource,
    /// The source said [`ActionSourceError::Unavailable`]: it does not serve this
    /// reference. Rejected like `NoSource`, scoped to the one reference; the
    /// reason, when the source knows one, tells the hint whether refreshing the
    /// source could ever help.
    Unavailable(Option<String>),
    Failed(String),
}

impl<'w, 'a> Lowering<'w, 'a> {
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
                    self.diags.unsupported(
                        "action.remote",
                        span.clone(),
                        name.to_string(),
                        &unavailable_hint(reason),
                    );
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
    pub(super) fn action_plan(&mut self, step: &Step<'_>) -> Option<ActionPlan> {
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
    pub(super) fn lifecycle_node(
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
    pub(super) fn uses_step(
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
