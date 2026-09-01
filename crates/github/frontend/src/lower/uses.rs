//! `uses:` steps: remote references pin statically. Every manifest-backed
//! action is planned when its resolver runs. `docker://` needs no manifest and
//! stays a direct node.

use std::collections::{BTreeMap, HashSet};
use std::mem;

use frontend::diag::{Diagnostics, Span};
use frontend::yaml::{Document, Node};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{BinOp, ExprId, NodeId, ScopeId, SplicePolicy, StepRef, UnOp, Value};
use serde_json::{Map, json};

use super::{
    ActionContext, ActionPlan, Lowering, PlanInput, PlanKind, scalar_text, scalar_text_opt,
};
use crate::action::{
    ACTION_KIND, ActionLocation, ActionRef, ActionSourceError, CHECKOUT_KIND, DEFERRED_ACTION_KIND,
    DEFERRED_ACTION_POST_KIND, DEFERRED_ACTION_PUBLISH_KIND, DOCKER_ACTION_KIND, Phase,
    PinnedAction, REPO_PARAM_CONTEXT, REPO_PARAM_KEY, RefError, STATE_OUTPUT_KEY, render_chain,
    unavailable_hint, validate_relative_action_path,
};
use crate::call::CalleeSource;
use crate::composite::{self, DockerAction, NodeAction, Runs, Uses};
use crate::exprs::{ExprSite, LoweredScalar, SEP, Sentinel, Site, config_value, lower_scalar};
use crate::gate::{self, GateOp};
use crate::model::{Defaults, Job, Step};

/// Why a remote action did not resolve.
#[derive(Clone)]
pub(super) enum ResolveFailure {
    /// No [`ActionSource`] was given: the format is running without one.
    NoSource,
    /// The source said [`ActionSourceError::Unavailable`]: it does not serve
    /// this reference. Rejected like `NoSource`, scoped to the one
    /// reference; the reason, when the source knows one, tells the hint
    /// whether refreshing the source could ever help.
    Unavailable(Option<String>),
    Failed(ResolveError),
}

/// The typed failure behind [`ResolveFailure::Failed`], kept whole so the
/// diagnostic renders the full chain.
#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum ResolveError {
    #[error(transparent)]
    Reference(#[from] RefError),
    #[error(transparent)]
    Source(#[from] ActionSourceError),
}

impl<'a> Lowering<'_, 'a> {
    /// Resolve `owner/repo@ref` to its commit, once per reference. The
    /// manifest stays a run-time lookup.
    fn resolve_remote(&mut self, name: &str) -> Result<PinnedAction, ResolveFailure> {
        if let Some(cached) = self.resolved.get(name) {
            return cached.clone();
        }
        let source_failure = |e: ActionSourceError| match e {
            ActionSourceError::Unavailable { reason, .. } => ResolveFailure::Unavailable(reason),
            other => ResolveFailure::Failed(other.into()),
        };
        let result = match self.actions {
            None => Err(ResolveFailure::NoSource),
            Some(source) => ActionRef::parse(name)
                .map_err(|e| ResolveFailure::Failed(e.into()))
                .and_then(|reference| source.resolve(&reference).map_err(source_failure)),
        };
        self.resolved.insert(name.to_string(), result.clone());
        result
    }

    /// The `action.yml` document for a `uses:` reference, or the diagnostic
    /// saying why there is none.
    fn action_document(&mut self, reference: &str, span: &Span) -> Option<Document> {
        match composite::classify(reference) {
            Uses::Local(path) => composite::read_document(self.files, &path, span, &mut self.diags),
            // `docker://` names an image, not files; the plan path handles it
            // before any document is asked for.
            Uses::Docker(_) => None,
            Uses::Remote(name) => match self.resolve_remote(&name) {
                Ok(pinned) => {
                    if let Some(text) = self
                        .actions
                        .and_then(|source| source.manifest(&pinned).ok())
                    {
                        Document::parse(
                            &format!("{}/action.yml", pinned.reference()),
                            &text,
                            &mut self.diags,
                        )
                    } else {
                        self.diags.error(
                            "action.unresolved",
                            span.clone(),
                            format!("`uses: {name}` has no readable action manifest"),
                        );
                        None
                    }
                }
                Err(failure) => {
                    self.report_resolve_failure(&name, span, failure);
                    None
                }
            },
        }
    }

    /// The plan for a `uses:` step that contributes standalone nodes — a
    /// JavaScript or Docker action; `None` for anything else, quietly — the
    /// main pass reports problems.
    pub(super) fn action_plan(&mut self, job_id: &str, step: &Step<'_>) -> Option<ActionPlan> {
        let (reference, span) = step.uses.as_ref()?;
        let eager = self.is_eager_root(job_id, &step.node_name());
        if !eager && !matches!(composite::classify(reference), Uses::Docker(_)) {
            return None;
        }
        let saved = mem::replace(&mut self.diags, Diagnostics::new());
        let plan = self.action_plan_inner(reference, span);
        let plan_diags = mem::replace(&mut self.diags, saved);
        if plan.is_some() {
            self.diags.extend(plan_diags);
        }
        plan
    }

    fn action_plan_inner(&mut self, reference: &str, span: &Span) -> Option<ActionPlan> {
        if let Uses::Docker(image) = composite::classify(reference) {
            return Some(docker_image_plan(&image));
        }
        let location = self.location_of(reference)?;
        let doc = self.action_document(reference, span)?;
        let manifest = composite::read_manifest(&doc, &mut self.diags)?;
        let kind = match manifest.runs {
            Runs::Node(node) => PlanKind::Node(node),
            Runs::Docker(docker) => PlanKind::Docker(docker),
            Runs::Composite(_) => return None,
        };
        Some(ActionPlan {
            location: Some(location),
            kind,
            inputs: plan_inputs(&manifest.inputs),
        })
    }

    /// Where a `uses:` reference's files are found at run time; `None` when it
    /// names no fetched or local tree.
    fn location_of(&mut self, reference: &str) -> Option<ActionLocation> {
        match composite::classify(reference) {
            Uses::Local(path) => Some(ActionLocation::Local { local: path }),
            Uses::Docker(_) => None,
            Uses::Remote(name) => Some(ActionLocation::Pinned(self.resolve_remote(&name).ok()?)),
        }
    }

    /// A `pre` or `post` node for a JavaScript or Docker action. `pre-if` and
    /// `post-if` default to `always()`; a `post` also needs its main node to
    /// have run.
    pub(super) fn lifecycle_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        phase: Phase,
    ) -> Option<NodeId> {
        let ActionContext { step, site, .. } = context;
        let (_, span) = step.uses.as_ref()?;
        if phase == Phase::Main {
            return None;
        }
        let source = plan.phase_if(phase).unwrap_or("always()");
        let main_name = format!("{}{SEP}{}", site.job_id, step.node_name());
        let state_from = (phase == Phase::Post).then(|| main_name.clone());
        let id = self.phase_node(context, plan, phase, state_from.as_deref());
        let mut prereqs = vec![site.job_started(self.b.exprs())];
        if phase == Phase::Post {
            prereqs.push(self.main_ran(site, &main_name));
        }
        let gate = self.step_gate_text(source, site, span.clone(), &prereqs);
        self.attach_gate(id, gate);
        Some(id)
    }

    /// One phase's node, whichever kind of action the plan holds.
    fn phase_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        phase: Phase,
        state_from: Option<&str>,
    ) -> NodeId {
        match &plan.kind {
            PlanKind::Node(node) => self.action_node(context, plan, node, phase, state_from),
            PlanKind::Docker(docker) => {
                self.docker_action_node(context, plan, docker, phase, state_from)
            }
        }
    }

    /// GitHub's runner warns and runs anyway when a step omits a required
    /// input; real workflows rely on that, so this is a warning everywhere.
    fn warn_missing_input(&mut self, uses: &str, input: &str, span: Span) {
        self.diags.warning(
            "gha.missing_input",
            span,
            format!("`{uses}` declares required input `{input}`, which this step does not provide"),
        );
    }

    /// One `github/action` node: the action pinned, its phase and entry point,
    /// its inputs and env lowered where the step is.
    fn action_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        action: &NodeAction,
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
            Phase::Pre => action.pre.clone(),
            Phase::Main => Some(action.main.clone()),
            Phase::Post => action.post.clone(),
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
        let with = lowercased_with(step);
        let mut inputs = Map::new();
        let mut declared: HashSet<String> = HashSet::new();
        for input in &plan.inputs {
            let key = input.name.to_lowercase();
            declared.insert(key.clone());
            let value = if let Some(node) = with.get(&key) {
                self.with_value(*node, &step_site)
            } else if let Some(text) = &input.default {
                self.text_value(text, span.clone(), &step_site)
            } else {
                if input.required && phase == Phase::Main {
                    self.warn_missing_input(uses, &input.name, span.clone());
                }
                None
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

        let location = plan
            .location
            .as_ref()
            .expect("a JavaScript action has files");
        let mut config = Map::new();
        config.insert(
            "action".into(),
            serde_json::to_value(location).expect("an action location serializes"),
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
        if let Some(v) =
            self.soft_fail_value(step.continue_on_error, &step_site, phase != Phase::Main)
        {
            config.insert("soft_fail".into(), v);
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

    /// One `github/docker_action` node: the image (registry or the action's
    /// Dockerfile), the phase's entrypoint, and — for the main phase — its
    /// args, with the action's declared inputs bound so the manifest's own
    /// `args` and `env` expressions read them.
    fn docker_action_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        plan: &ActionPlan,
        docker: &DockerAction,
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
        let suffix = match phase {
            Phase::Pre => format!("{SEP}pre"),
            Phase::Main => String::new(),
            Phase::Post => format!("{SEP}post"),
        };
        let node_name = format!("{}{SEP}{}{suffix}", site.job_id, step.node_name());

        let mut step_site = site.clone();
        let mut env_config = self.step_env_config(step, &mut step_site, job_secret_env);

        // `with:` keys lowercased once, for the input match and the main-phase
        // entrypoint and args overrides below.
        let with = lowercased_with(step);

        // Declared inputs (or their defaults) as expressions: the `INPUT_*`
        // values, and the `inputs` context the manifest's own text lowers in.
        let inputs = self.docker_action_inputs(plan, step, &with, &step_site, &span, uses, phase);
        let mut manifest_site = step_site.clone();
        manifest_site.action_inputs = Some(inputs.clone());

        let mut config = Map::new();
        if let Some(registry) = docker.image.strip_prefix("docker://") {
            config.insert("image".into(), json!({ "registry": registry }));
        } else {
            let location = plan
                .location
                .as_ref()
                .expect("a Dockerfile action has files");
            config.insert(
                "image".into(),
                json!({ "dockerfile": {
                    "action": serde_json::to_value(location)
                        .expect("an action location serializes"),
                    "file": docker.image,
                }}),
            );
        }

        // `with.entrypoint` and `with.args` override the manifest for the main
        // phase, as GitHub documents for Docker container actions; `pre` and
        // `post` run their own entrypoints with no args.
        let entrypoint = match phase {
            Phase::Pre => docker
                .pre_entrypoint
                .as_ref()
                .and_then(|t| self.sentinel_text_value(t, span.clone(), &manifest_site)),
            Phase::Post => docker
                .post_entrypoint
                .as_ref()
                .and_then(|t| self.sentinel_text_value(t, span.clone(), &manifest_site)),
            Phase::Main => match with.get("entrypoint") {
                Some(node) => self.sentinel_with_value(*node, &step_site),
                None => docker
                    .entrypoint
                    .as_ref()
                    .and_then(|t| self.sentinel_text_value(t, span.clone(), &manifest_site)),
            },
        };
        if let Some(value) = entrypoint {
            config.insert("entrypoint".into(), value);
        }
        if phase == Phase::Main {
            match with.get("args") {
                // One string, shell-split by the step after expressions and
                // secrets resolve, as GitHub does.
                Some(node) => {
                    if let Some(value) = self.sentinel_with_value(*node, &step_site) {
                        config.insert("args_text".into(), value);
                    }
                }
                None if !docker.args.is_empty() => {
                    let mut args = Vec::new();
                    for arg in &docker.args {
                        if let Some(value) =
                            self.sentinel_text_value(arg, span.clone(), &manifest_site)
                        {
                            args.push(value);
                        }
                    }
                    config.insert("args".into(), Value::Array(args));
                }
                None => {}
            }
        }

        let mut inputs_config = Map::new();
        for (name, id) in &inputs {
            inputs_config.insert(name.clone(), json!({ EXPR_PLACEHOLDER_KEY: id.raw() }));
        }
        config.insert("inputs".into(), Value::Object(inputs_config));

        // The manifest's `runs.env`, under the step's own `env:`.
        for (key, text) in &docker.env {
            if env_config.contains_key(key.as_str()) {
                continue;
            }
            if let Some(lowered) = lower_scalar(
                text,
                span.clone(),
                &manifest_site,
                ExprSite::Step,
                true,
                self.b.exprs(),
                &mut self.diags,
            ) {
                env_config.insert(key.clone(), config_value(lowered));
            }
        }
        if !env_config.is_empty() {
            config.insert("env".into(), Value::Object(env_config));
        }
        config.insert("event".into(), self.event_config());
        if let Some(from) = state_from {
            config.insert("state".into(), self.state_config(site, from));
        }
        if let Some(v) =
            self.soft_fail_value(step.continue_on_error, &step_site, phase != Phase::Main)
        {
            config.insert("soft_fail".into(), v);
        }

        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(DOCKER_ACTION_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        id
    }

    /// A Docker action's inputs, each as an expression: the caller's `with:`
    /// (or the declared default), plus undeclared `with:` keys, which pass
    /// through as GitHub's do. A whole-value secret rides as its sentinel,
    /// resolved by the step at spawn. Missing required inputs are reported on
    /// the main phase only, like a JavaScript action's.
    fn docker_action_inputs(
        &mut self,
        plan: &ActionPlan,
        step: &Step<'_>,
        with: &BTreeMap<String, Node<'_>>,
        site: &Site,
        span: &Span,
        uses: &str,
        phase: Phase,
    ) -> BTreeMap<String, ExprId> {
        let mut inputs: BTreeMap<String, ExprId> = BTreeMap::new();
        let mut declared: HashSet<String> = HashSet::new();
        for input in &plan.inputs {
            let key = input.name.to_lowercase();
            declared.insert(key.clone());
            let id = if let Some(node) = with.get(&key) {
                self.input_expr_from_node(*node, site)
            } else if let Some(text) = &input.default {
                self.input_expr_from_text(text, span.clone(), site)
            } else {
                if input.required && phase == Phase::Main {
                    self.warn_missing_input(uses, &input.name, span.clone());
                }
                None
            };
            if let Some(id) = id {
                inputs.insert(input.name.clone(), id);
            }
        }
        for (key, node) in &step.with {
            if !declared.contains(&key.to_lowercase())
                && let Some(id) = self.input_expr_from_node(*node, site)
            {
                inputs.insert(key.clone(), id);
            }
        }
        inputs
    }

    fn input_expr_from_node(&mut self, node: Node<'_>, site: &Site) -> Option<ExprId> {
        if let Some(text) = node.as_str() {
            self.input_expr_from_text(text, node.span(), site)
        } else {
            let text = scalar_text(node);
            Some(self.b.exprs().lit(text))
        }
    }

    fn input_expr_from_text(&mut self, text: &str, span: Span, site: &Site) -> Option<ExprId> {
        match lower_scalar(
            text,
            span,
            site,
            ExprSite::Step,
            true,
            self.b.exprs(),
            &mut self.diags,
        )? {
            LoweredScalar::Literal(v) => Some(self.b.exprs().lit(v)),
            LoweredScalar::Expr(id) => Some(id),
            LoweredScalar::Secret(name) => {
                let sentinel = Sentinel::secret(&name);
                Some(self.b.exprs().lit(sentinel))
            }
        }
    }

    /// Lower text for a docker action's args or entrypoint, where a whole-value
    /// secret rides as its sentinel — these positions are not env-shaped maps,
    /// so a `$secret` reference has nowhere to live.
    fn sentinel_text_value(&mut self, text: &str, span: Span, site: &Site) -> Option<Value> {
        let lowered = lower_scalar(
            text,
            span,
            site,
            ExprSite::Step,
            true,
            self.b.exprs(),
            &mut self.diags,
        )?;
        Some(match lowered {
            LoweredScalar::Secret(name) => Value::String(Sentinel::secret(&name)),
            other => config_value(other),
        })
    }

    fn sentinel_with_value(&mut self, node: Node<'_>, site: &Site) -> Option<Value> {
        match node.as_str() {
            Some(text) => self.sentinel_text_value(text, node.span(), site),
            None => Some(Value::String(scalar_text(node))),
        }
    }

    /// Is this step a supportable `actions/checkout` call — one the local
    /// substitution can honor? `Some(path)` when it is, carrying the literal
    /// `path:` when given. The rules, adopted from the plan (and act's
    /// spelled-out-default rule): default inputs, plus a literal `path:`;
    /// `fetch-depth`/`persist-credentials` accepted and ignored (local-clone
    /// policy); a literal `repository:`/`ref:` *equal to the checkout's own*
    /// still substitutes, since real workflows spell the default out. Anything
    /// else — another repository, a non-matching or expression value,
    /// `submodules:`, a `token:` — falls through to the real action, which
    /// needs real credentials. No diagnostics: the fall-through is the design.
    #[expect(
        clippy::option_option,
        reason = "the two levels answer different questions: the outer whether the step is a \
                  substitutable checkout at all, the inner whether it named a literal `path:`. \
                  One `Option` would lose the difference"
    )]
    pub(super) fn substitutable_checkout(&self, step: &Step<'_>) -> Option<Option<String>> {
        if !self.substitute_checkout {
            return None;
        }
        let (reference, _) = step.uses.as_ref()?;
        let Uses::Remote(name) = composite::classify(reference) else {
            return None;
        };
        let bare = name.split('@').next().unwrap_or(&name);
        if bare != "actions/checkout" {
            return None;
        }
        let mut path = None;
        for (key, node) in lowercased_with(step) {
            // Only whole literal values qualify; any expression falls through.
            let literal = node
                .as_str()
                .filter(|text| !text.contains("${{"))
                .map(str::to_string);
            match key.as_str() {
                "fetch-depth" | "persist-credentials" => {}
                "path" => path = Some(literal?),
                "repository" => {
                    let own = self
                        .github_identity
                        .get("repository")
                        .and_then(Value::as_str);
                    if own.is_none() || literal.as_deref() != own {
                        return None;
                    }
                }
                "ref" => {
                    let matches = [
                        self.github_identity.get("ref").and_then(Value::as_str),
                        self.github_identity.get("ref_name").and_then(Value::as_str),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|own| literal.as_deref() == Some(own));
                    if !matches {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        Some(path)
    }

    /// The `github/checkout` node a substitutable call becomes: source is the
    /// host-filled `petri.repo` run parameter, resolved at firing — a run
    /// whose host filled nothing fails the step routably, never the lowering.
    fn checkout_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        earlier: &[String],
        path: Option<String>,
    ) -> Vec<NodeId> {
        let ActionContext {
            job,
            step,
            scope,
            site,
            job_secret_env: _,
        } = context;
        let path = path.filter(|p| !p.is_empty());
        let mut step_site = site.clone();
        step_site.earlier_steps = earlier.to_vec();
        let node_name = format!("{}{SEP}{}", site.job_id, step.node_name());

        let source = {
            let t = self.b.exprs();
            let petri = t.var(REPO_PARAM_CONTEXT);
            let key = t.lit(REPO_PARAM_KEY);
            t.call("get_ci", vec![petri, key])
        };
        let mut config = Map::new();
        config.insert(
            "source".into(),
            json!({ EXPR_PLACEHOLDER_KEY: source.raw() }),
        );
        // Run identity, resolved at firing like `source`: the step shapes the
        // snapshot's git state to answer ordinary questions the way GitHub's
        // checkout would — the ref's branch checked out, `origin` naming the
        // repository.
        for key in ["ref", "repository", "server_url"] {
            let value = {
                let t = self.b.exprs();
                let github = t.var("github");
                let k = t.lit(key);
                t.call("get_ci", vec![github, k])
            };
            config.insert(key.into(), json!({ EXPR_PLACEHOLDER_KEY: value.raw() }));
        }
        if let Some(path) = path {
            config.insert("path".into(), json!(path));
        }
        if let Some(v) = self.soft_fail_value(step.continue_on_error, &step_site, false) {
            config.insert("soft_fail".into(), v);
        }
        let id = self.b.add_node(
            &node_name,
            scope,
            StepRef::new(CHECKOUT_KIND, Value::Object(config)),
        );
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        self.gate_main_node(id, step, &step_site);
        vec![id]
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
        let state_from = (context.site.action_inputs.is_none() && plan.has_pre())
            .then(|| format!("{main_name}{SEP}pre"));
        let main_context = ActionContext {
            site: &step_site,
            ..context
        };
        let id = self.phase_node(main_context, plan, Phase::Main, state_from.as_deref());
        self.gate_main_node(id, context.step, &step_site);
        vec![id]
    }

    /// A `with:` value: a string, possibly templated; anything else
    /// stringified.
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
            ExprSite::Step,
            true,
            self.b.exprs(),
            &mut self.diags,
        )
        .map(config_value)
    }

    /// Lower one `uses:` step. Normal workflow lowering emits a deferred
    /// resolver. The run-time planner eagerly expands only the reached root
    /// action; nested actions become deferred resolvers of their own.
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
        if let Some(path) = self.substitutable_checkout(step) {
            return self.checkout_node(
                ActionContext {
                    job,
                    step,
                    scope,
                    site,
                    job_secret_env,
                },
                earlier,
                path,
            );
        }
        let eager = self.is_eager_root(&site.job_id, &step.node_name());
        if !eager && !matches!(composite::classify(reference), Uses::Docker(_)) {
            return self.deferred_action_nodes(
                ActionContext {
                    job,
                    step,
                    scope,
                    site,
                    job_secret_env,
                },
                reference,
                span,
                earlier,
                depth,
            );
        }
        // Inside a composite, plans are not precomputed: a `docker://` step
        // builds its plan inline, and a manifest-backed action builds one from
        // the manifest below.
        let inline_plan = match action_plan {
            None => match composite::classify(reference) {
                Uses::Docker(image) => Some(docker_image_plan(&image)),
                _ => None,
            },
            Some(_) => None,
        };
        if let Some(plan) = action_plan.or(inline_plan.as_ref()) {
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
            // The manifest in hand is the plan; no need to read it again.
            runs @ (Runs::Node(_) | Runs::Docker(_)) => {
                let Some(location) = self.location_of(reference) else {
                    return Vec::new();
                };
                let kind = match runs {
                    Runs::Node(node) => PlanKind::Node(node),
                    Runs::Docker(docker) => PlanKind::Docker(docker),
                    Runs::Composite(_) => unreachable!("matched above"),
                };
                let plan = ActionPlan {
                    location: Some(location),
                    kind,
                    inputs: plan_inputs(&manifest.inputs),
                };
                let nested = depth > 0 || self.runtime_site().is_some_and(|site| site.depth > 0);
                if nested && plan.has_post() {
                    self.diags.warning(
                        "action.nested_lifecycle",
                        span.clone(),
                        format!(
                            "`{reference}` has a `post` step, which does not run inside a composite action here; its `pre` and `main` steps do"
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
        };
        // Inputs: the caller's `with`, lowered in the caller's site, else the default.
        let caller_site = {
            let mut s = site.clone();
            s.earlier_steps = earlier.to_vec();
            s
        };
        let mut inputs: BTreeMap<String, ExprId> = BTreeMap::new();
        let with = lowercased_with(step);
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
                        ExprSite::Step,
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
                            let id = self.b.exprs().lit(Sentinel::secret(&name));
                            inputs.insert(input.name.clone(), id);
                        }
                        None => {}
                    }
                }
                None if input.required => {
                    self.warn_missing_input(reference, &input.name, span.clone());
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
                shell:             inner.shell,
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
                inner_site.earlier_steps.clone_from(&names_so_far);
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
                ExprSite::Step,
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

    /// A manifest-backed action stays one resolver plus one public publisher
    /// in the static graph. The resolver reads and plans the action when this
    /// step is reached. Its appended fragment ends at a private result, which
    /// makes the publisher wait and then reproduce the real outcome.
    fn deferred_action_nodes(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
        reference: &str,
        span: &Span,
        earlier: &[String],
        depth: usize,
    ) -> Vec<NodeId> {
        let ActionContext {
            job,
            step,
            scope,
            site,
            job_secret_env,
        } = context;
        let action = match composite::classify(reference) {
            Uses::Local(local) => {
                let result = match &self.frames[self.current].source {
                    CalleeSource::Remote { pinned } => pinned
                        .at_repository_path(&local)
                        .map(ActionLocation::Pinned),
                    CalleeSource::Root | CalleeSource::Local => {
                        validate_relative_action_path(&local, true)
                            .map(|()| ActionLocation::Local { local })
                    }
                };
                match result {
                    Ok(action) => action,
                    Err(error) => {
                        self.diags
                            .error("gha.bad_action_path", span.clone(), error.to_string());
                        return Vec::new();
                    }
                }
            }
            Uses::Remote(name) => match self.resolve_remote(&name) {
                Ok(pinned) => ActionLocation::Pinned(pinned),
                Err(failure) => {
                    self.report_resolve_failure(&name, span, failure);
                    return Vec::new();
                }
            },
            Uses::Docker(_) => unreachable!("docker image actions are planned eagerly"),
        };

        let mut step_site = site.clone();
        step_site.earlier_steps = earlier.to_vec();
        let env = self.step_env_config(step, &mut step_site, job_secret_env);
        let mut with = Map::new();
        for (name, node) in &step.with {
            if let Some(value) = self.with_value(*node, &step_site) {
                with.insert(name.clone(), value);
            }
        }

        let public_name = format!("{}{SEP}{}", site.job_id, step.node_name());
        let resolver_name = format!("{public_name}{SEP}resolve");
        let result_name = format!("{public_name}{SEP}runtime-result");
        let mut config = Map::new();
        config.insert(
            "action".into(),
            serde_json::to_value(action).expect("an action location serializes"),
        );
        config.insert("job_id".into(), json!(site.job_id));
        config.insert("start_node".into(), json!(site.start_node));
        config.insert("step_id".into(), json!(step.node_name()));
        config.insert("result_name".into(), json!(result_name));
        config.insert("with".into(), Value::Object(with));
        config.insert("env".into(), Value::Object(env));
        config.insert("event".into(), self.event_config());
        config.insert("matrix".into(), json!(site.matrix));
        config.insert("in_expansion".into(), json!(site.in_expansion));
        if site.matrix || site.in_expansion {
            // The same `index` binding the publisher's `node_record` reads;
            // the step needs it to name its materialized result node.
            let index = self.b.exprs().var("index");
            config.insert("index".into(), json!({ EXPR_PLACEHOLDER_KEY: index.raw() }));
        }
        config.insert("needs".into(), json!(site.needs));
        let base_depth = self.runtime_site().map_or(0, |site| site.depth);
        config.insert("depth".into(), json!(base_depth + depth));
        if let Some(value) = self.soft_fail_value(step.continue_on_error, &step_site, false) {
            config.insert("soft_fail".into(), value);
        }
        let tolerates_failure = job
            .continue_on_error
            .and_then(|node| node.as_scalar())
            .and_then(|scalar| scalar.as_bool())
            .unwrap_or(false);
        config.insert("tolerates_failure".into(), json!(tolerates_failure));
        if let Some(timeout) = step.timeout_minutes.or(job.timeout_minutes) {
            config.insert("timeout_minutes".into(), json!(scalar_text(timeout)));
        }

        let resolver = self.b.add_node(
            &resolver_name,
            scope,
            StepRef::new(DEFERRED_ACTION_KIND, Value::Object(config)),
        );
        self.b.node_mut(resolver).splice_policy = SplicePolicy::Append;
        self.spans.insert(resolver, step.span.clone());
        self.set_step_budget(resolver, job, step);
        let started = step_site.job_started(self.b.exprs());
        let gate = self.step_gate(step.condition, &step_site, span.clone(), &[started]);
        self.attach_gate(resolver, gate);

        let result = {
            let t = self.b.exprs();
            let record = site.node_record(t, &result_name);
            let output = t.field(record, "output");
            json!({ EXPR_PLACEHOLDER_KEY: output.raw() })
        };
        let mut publish_config = Map::new();
        publish_config.insert("result".into(), result);
        if let Some(value) = self.soft_fail_value(step.continue_on_error, &step_site, true) {
            publish_config.insert("soft_fail".into(), value);
        }
        let publisher = self.b.add_node(
            &public_name,
            scope,
            StepRef::new(DEFERRED_ACTION_PUBLISH_KIND, Value::Object(publish_config)),
        );
        self.spans.insert(publisher, step.span.clone());
        vec![resolver, publisher]
    }

    fn report_resolve_failure(&mut self, name: &str, span: &Span, failure: ResolveFailure) {
        match failure {
            ResolveFailure::NoSource => self.diags.unsupported(
                "action.remote",
                span.clone(),
                name,
                "no action source is configured, so actions from other repositories cannot be fetched",
            ),
            // A recorded upstream refusal — the repository is private or
            // removed — is its own class: the workflow is broken on GitHub
            // itself, and no refresh will change that. A reference the
            // source merely does not cover stays `action.remote`.
            ResolveFailure::Unavailable(reason) => {
                let code = if reason.is_some() {
                    "action.upstream_gone"
                } else {
                    "action.remote"
                };
                self.diags.unsupported(code, span.clone(), name, &unavailable_hint(reason));
            }
            ResolveFailure::Failed(error) => self.diags.error(
                "action.unresolved",
                span.clone(),
                format!("`uses: {name}`: {}", render_chain(&error)),
            ),
        }
    }

    /// The cleanup dispatcher for a deferred action. It is always present
    /// because the manifest is not known yet. A manifest with no post phase
    /// gives it no request, so it finishes as a no-op.
    pub(super) fn deferred_action_post_node(
        &mut self,
        context: ActionContext<'_, 'a, '_>,
    ) -> Option<NodeId> {
        let ActionContext {
            job,
            step,
            scope,
            site,
            ..
        } = context;
        let (reference, _) = step.uses.as_ref()?;
        if matches!(composite::classify(reference), Uses::Docker(_))
            || self.substitutable_checkout(step).is_some()
        {
            return None;
        }
        let public_name = format!("{}{SEP}{}", site.job_id, step.node_name());
        let resolver_name = format!("{public_name}{SEP}resolve");
        let request = {
            let t = self.b.exprs();
            let record = site.node_record(t, &resolver_name);
            let output = t.field(record, "output");
            let post = t.field(output, "post");
            json!({ EXPR_PLACEHOLDER_KEY: post.raw() })
        };
        let id = self.b.add_node(
            &format!("{public_name}{SEP}post-resolve"),
            scope,
            StepRef::new(DEFERRED_ACTION_POST_KIND, json!({ "request": request })),
        );
        self.b.node_mut(id).splice_policy = SplicePolicy::Append;
        self.spans.insert(id, step.span.clone());
        self.set_step_budget(id, job, step);
        Some(id)
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

    /// The state an earlier phase of the same action saved, read from its
    /// record.
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

    /// `true` when the action's main node has a record other than `skipped`:
    /// the condition for its `post` to run at all.
    fn main_ran(&mut self, site: &Site, main: &str) -> ExprId {
        let t = self.b.exprs();
        let status = site.node_status(t, main);
        let skipped = t.lit("skipped");
        let status = t.call("default", vec![status, skipped]);
        let is_skipped = t.binary(BinOp::Eq, status, skipped);
        t.unary(UnOp::Not, is_skipped)
    }
}

/// A step's `with:` keys lowercased, as GitHub matches inputs.
fn lowercased_with<'n>(step: &Step<'n>) -> BTreeMap<String, Node<'n>> {
    step.with
        .iter()
        .map(|(k, v)| (k.to_lowercase(), *v))
        .collect()
}

/// The plan for `uses: docker://image`: no files, no manifest — the image as
/// written, and whatever `with:` provides.
fn docker_image_plan(image: &str) -> ActionPlan {
    ActionPlan {
        location: None,
        kind:     PlanKind::Docker(DockerAction {
            image:           format!("docker://{image}"),
            entrypoint:      None,
            pre_entrypoint:  None,
            pre_if:          None,
            post_entrypoint: None,
            post_if:         None,
            args:            Vec::new(),
            env:             Vec::new(),
        }),
        inputs:   Vec::new(),
    }
}

/// A manifest's declared inputs, owned, so the plan outlives the document.
fn plan_inputs(inputs: &[composite::Input<'_>]) -> Vec<PlanInput> {
    inputs
        .iter()
        .map(|input| PlanInput {
            name:     input.name.clone(),
            default:  input.default.and_then(scalar_text_opt),
            required: input.required,
        })
        .collect()
}
