//! Fabro workflow → HIR.
//!
//! One pass over the semantic [`Workflow`]: node kinds and step configs,
//! then routing — one tiered group per node, the four Fabro tiers — then the
//! implicit edges (goal gates), back-edge classification, joins and budgets,
//! and finally the engine's own validation mapped back onto source spans.
//! Every construct here lowers onto what the core has; nothing invents engine
//! semantics.

mod attrs;
mod compaction;
mod hooks;
mod imports;
mod parallel;
pub(crate) mod policy;
mod promotion;
mod routing;
mod secrets;
mod threads;
mod workflow_toml;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Duration;

use frontend::{CompileInputs, Diagnostics, FileSource, Lowered, Span};
pub use compaction::{CompactionSettings, DEFAULT_PRESERVE_TURNS, DEFAULT_THRESHOLD_PERCENT};
pub use imports::IMPORT_ERROR;
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::validate::loop_reachable;
use ir::{
    Budget, Completion, Edge, EdgeId, ExprId, GraphBuilder, JoinPolicy, NodeId, Routing, Scope,
    ScopeId, StepRef,
};
pub use parallel::{BRANCH_META_KIND, DEFAULT_MAX_PARALLEL};
pub use policy::MAX_INVOCATIONS;
pub use promotion::ROUTES_KEY;
pub use routing::{FailurePolicy, Policy};
use serde_json::{Map, Value, json};
use smol_str::SmolStr;
pub use workflow_toml::{
    ENVIRONMENT_PARAM, EnvValue, Environment, LAUNCH_PARAM, ModelDefaults, PREPARE_NODE_PREFIX,
    PrepareStep, RunSettings,
};

use crate::hooks::HookDefinition;
use crate::kinds::{
    AGENT_KIND, COMMAND_KIND, GOAL_CHECK_NODE, HUMAN_KIND, MAX_OUTPUT_RETRIES, PROMPT_KIND,
    STAGE_KIND, WAIT_KIND, WORKFLOW_KIND,
};
use crate::model::{Attrs, EdgeDecl, NodeDecl, Workflow};
use crate::template::{self, Context, TemplateError};
use crate::{condition, dot, labels, model, stylesheet};

/// The hard maximum on firings of any node in a loop, and the value Fabro's
/// "unlimited" lowers to.
pub const MAX_FIRINGS: u32 = 500;

/// The most items a `for_each` fan-out may expand, as Fabro caps it.
pub const MAX_FOR_EACH_ITEMS: u64 = 1_000;

/// How deep nested workflows may go below the root — the GitHub frontend's
/// reusable-workflow limit, shared so the two formats agree.
pub const MAX_CALL_DEPTH: usize = 3;

/// Default per-attempt timeouts where Fabro has one, or where the engine
/// needs a finite one.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
const AGENT_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
const HUMAN_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 3600);
const STRUCTURAL_TIMEOUT: Duration = Duration::from_secs(3600);

/// The kinds of node a Fabro graph has, by shape or `type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Start,
    Exit,
    Conditional,
    Agent,
    Prompt,
    Command,
    Human,
    Parallel,
    FanIn,
    Wait,
    ManagerLoop,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Exit => "exit",
            Self::Conditional => "conditional",
            Self::Agent => "agent",
            Self::Prompt => "prompt",
            Self::Command => "command",
            Self::Human => "human",
            Self::Parallel => "parallel",
            Self::FanIn => "parallel.fan_in",
            Self::Wait => "wait",
            Self::ManagerLoop => "stack.manager_loop",
        }
    }

    fn from_type(name: &str) -> Option<Self> {
        Some(match name {
            "start" => Self::Start,
            "exit" => Self::Exit,
            "conditional" => Self::Conditional,
            "agent" => Self::Agent,
            "prompt" => Self::Prompt,
            "command" | "tool" => Self::Command,
            "human" => Self::Human,
            "parallel" => Self::Parallel,
            "parallel.fan_in" => Self::FanIn,
            "wait" => Self::Wait,
            "stack.manager_loop" => Self::ManagerLoop,
            _ => return None,
        })
    }

    fn from_shape(shape: &str) -> Option<Self> {
        Some(match shape {
            "Mdiamond" => Self::Start,
            "Msquare" => Self::Exit,
            "box" => Self::Agent,
            "tab" => Self::Prompt,
            "hexagon" => Self::Human,
            "diamond" => Self::Conditional,
            "component" => Self::Parallel,
            "tripleoctagon" => Self::FanIn,
            "parallelogram" => Self::Command,
            "house" => Self::ManagerLoop,
            "insulator" => Self::Wait,
            _ => return None,
        })
    }

    fn is_llm(self) -> bool {
        matches!(self, Self::Agent | Self::Prompt)
    }
}

/// The node's Graphviz shape: as written, else inferred from a `script`,
/// else `box`. Public for the stylesheet's shape selectors.
pub fn shape_of(node: &NodeDecl) -> String {
    if let Some(shape) = node.attrs.text("shape") {
        return shape;
    }
    if !node.attrs.contains("type") && node.attrs.contains("script") {
        return "parallelogram".into();
    }
    "box".into()
}

/// One node, resolved.
struct Resolved {
    kind:   Kind,
    id:     NodeId,
    policy: FailurePolicy,
}

struct Structure {
    start: String,
    exit:  String,
}

struct Ctx<'a> {
    files:            &'a dyn FileSource,
    diags:            Diagnostics,
    b:                GraphBuilder,
    scope:            ScopeId,
    /// Fabro node id → engine node.
    ids:              HashMap<String, NodeId>,
    spans:            HashMap<NodeId, Span>,
    kinds:            HashMap<String, Kind>,
    /// The directory of the workflow file, for `@file` references.
    base_dir:         String,
    template:         Context,
    goal:             String,
    /// The graph-wide edge-selection mode.
    random:           bool,
    /// One `info.budget.default` per graph.
    budget_defaulted: bool,
    /// Pre-lowered child workflows, root first.
    children:         Vec<ir::Graph>,
    /// The chain of workflow files being lowered, root first, for cycle and
    /// depth checks on nested workflows.
    stack:            Vec<String>,
    /// Whether an unbound template input is a warning that leaves the text
    /// unrendered: `petri check` with no inputs. A run is always strict.
    lenient_unbound:  bool,
    /// What `workflow.toml` asked of the run.
    settings:         workflow_toml::RunSettings,
    /// The env each synthetic `[run.prepare]` command node carries.
    prepare_envs:     BTreeMap<String, BTreeMap<String, workflow_toml::EnvValue>>,
    /// The run's merged `[[run.hooks]]`, carried on the stage steps.
    hooks:            Vec<HookDefinition>,
    /// The workflow's name, `FABRO_WORKFLOW` for hooks.
    workflow_name:    String,
}

/// Lower a semantic workflow. `file` is the name spans carry; `files` reads
/// `@file` references and child workflows relative to the repository root.
pub(crate) fn lower(
    workflow: Workflow,
    file: &str,
    files: &dyn FileSource,
    inputs: &CompileInputs,
    diags: Diagnostics,
) -> Lowered {
    lower_nested(workflow, file, files, inputs, diags, Vec::new())
}

/// [`lower`] for a workflow `stack` deep in nested-workflow calls.
fn lower_nested(
    mut workflow: Workflow,
    file: &str,
    files: &dyn FileSource,
    inputs: &CompileInputs,
    mut diags: Diagnostics,
    stack: Vec<String>,
) -> Lowered {
    let mut template = Context::new(inputs);
    // A nested workflow shares its parent's run settings; only the root reads
    // the file beside it.
    let settings = if stack.is_empty() {
        workflow_toml::read(file, files, &mut template, &mut diags)
    } else {
        workflow_toml::RunSettings::default()
    };
    let hooks = if stack.is_empty() {
        hooks::load(files, inputs, settings.hooks_text.as_ref(), &mut diags)
    } else {
        Vec::new()
    };

    let mut b = GraphBuilder::bare();
    let scope = b.add_scope(Scope::new(ScopeId::new(0)));
    let base_dir = match file.rfind('/') {
        Some(index) => file[..index].to_string(),
        None => String::new(),
    };
    // Imports expand first, so everything below sees the spliced workflow.
    imports::expand(&mut workflow, file, &base_dir, &base_dir, files, &mut diags);
    let mut ctx = Ctx {
        files,
        diags,
        b,
        scope,
        ids: HashMap::new(),
        spans: HashMap::new(),
        kinds: HashMap::new(),
        base_dir,
        template,
        goal: String::new(),
        random: false,
        budget_defaulted: false,
        children: Vec::new(),
        stack,
        lenient_unbound: inputs.unbound_is_warning,
        settings,
        prepare_envs: BTreeMap::new(),
        hooks,
        workflow_name: workflow.name.clone(),
    };
    ctx.stack.push(file.to_string());

    ctx.graph_attrs(&mut workflow);
    let run_policy = policy::run_policy(&workflow, &mut ctx.diags);
    let Some(structure) = ctx.structure(&workflow) else {
        return Lowered::rejected(ctx.diags);
    };
    if ctx.stack.len() == 1 {
        let span = Span::file(&ctx.workflow_toml_path());
        ctx.prepare_envs = workflow_toml::insert_prepare_nodes(
            &mut workflow,
            &structure.start,
            &ctx.settings,
            &span,
            &mut ctx.diags,
        );
        ctx.environment_scope();
    }
    ctx.kinds(&workflow, &structure);

    // Pass 1: ids, in declaration order.
    for node in &workflow.nodes {
        let id = ctx
            .b
            .add_node(&node.id, ctx.scope, StepRef::new("noop", Value::Null));
        ctx.ids.insert(node.id.clone(), id);
        ctx.spans.insert(id, node.span.clone());
    }
    // Pass 2: steps and configs.
    let mut resolved = Vec::with_capacity(workflow.nodes.len());
    for node in &workflow.nodes {
        resolved.push(ctx.node(node, &workflow));
    }
    // Pass 3: routing, then the goal gate, then back edges, joins and budgets.
    let exit = ctx.ids[&structure.exit];
    let goal_check = ctx.goal_check(&workflow, exit);
    for (node, res) in workflow.nodes.iter().zip(&resolved) {
        ctx.routing(node, res, &workflow, exit, goal_check);
    }
    ctx.parallel(&workflow, &resolved, exit, goal_check);
    let start = ctx.ids[&structure.start];
    ctx.b.mark_entry(start);
    ctx.back_edges(start);
    ctx.joins_and_budgets(&workflow, &resolved, goal_check);

    if ctx.diags.has_errors() {
        return Lowered::rejected(ctx.diags);
    }
    let params = ctx.params();
    let Ctx {
        mut diags,
        b,
        spans,
        children,
        ..
    } = ctx;
    let mut graph = b.build();
    graph.completion = Completion::TerminalNode(exit);
    graph.policy = run_policy;
    graph.params = params;

    let report = ir::check(&graph);
    for error in &report.errors {
        let span = error
            .primary_node()
            .and_then(|node| spans.get(&node).cloned())
            .unwrap_or_else(|| Span::file(file));
        let mut d = frontend::Diagnostic::error(error.code(), span, error.to_string());
        if let Some(hint) = error.hint() {
            d = d.with_hint(hint);
        }
        diags.push(d);
    }
    for warning in &report.warnings {
        let span = spans
            .get(&warning.primary_node())
            .cloned()
            .unwrap_or_else(|| Span::file(file));
        let mut d = frontend::Diagnostic::warning(warning.code(), span, warning.to_string());
        if let Some(hint) = warning.hint() {
            d = d.with_hint(hint);
        }
        diags.push(d);
    }
    Lowered::with_children(graph, children, diags)
}

fn placeholder(id: ExprId) -> Value {
    json!({ EXPR_PLACEHOLDER_KEY: id.raw() })
}

fn duration_ms(duration: Duration) -> Value {
    Value::from(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

impl Ctx<'_> {
    /// The run parameters every graph this lowering produces carries: the
    /// launch settings on the root, the rendered inputs and vars, the goal.
    /// A branch child graph carries the same set, so an expression reads the
    /// same statics inside a branch.
    fn params(&self) -> BTreeMap<SmolStr, Value> {
        let mut params = BTreeMap::new();
        if self.stack.len() == 1 {
            params.insert(SmolStr::new(hooks::PARAM), hooks::param(&self.hooks));
            params.insert(SmolStr::new(LAUNCH_PARAM), self.settings.launch_param());
            if let Some(environment) = self.settings.environment_param() {
                params.insert(SmolStr::new(ENVIRONMENT_PARAM), environment);
            }
        }
        params.insert(
            SmolStr::new("inputs"),
            Value::Object(
                self.template
                    .inputs()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
        params.insert(
            SmolStr::new("vars"),
            Value::Object(
                self.template
                    .vars()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
        params.insert(SmolStr::new("goal"), Value::String(self.goal.clone()));
        params.insert(
            SmolStr::new("fabro_workflow"),
            Value::String(self.workflow_name.clone()),
        );
        params
    }

    fn unknown_attrs(
        &mut self,
        attrs: &Attrs,
        known: &[&str],
        ignored: &[(&str, &str)],
        what: &str,
    ) {
        for (key, attr) in attrs.iter() {
            if attrs::ATTRACTOR.contains(&key) {
                self.diags.unsupported(
                    "attractor",
                    attr.span.clone(),
                    format!(
                        "`{key}` is an Attractor-dialect attribute, which Fabro no longer reads"
                    ),
                    "use the Fabro spelling: `prompt` on a `box` node, `shape` for the kind",
                );
                continue;
            }
            if key == "auto_status" {
                // Fabro's `auto_status_deprecated` rule: accepted, warned.
                let message = if attrs.contains("on_failure") {
                    format!(
                        "`auto_status` on {what} is deprecated and ignored because `on_failure` \
                         is set"
                    )
                } else if attrs.bool("auto_status", &mut Diagnostics::new()) == Some(true) {
                    format!(
                        "`auto_status=true` on {what} is the deprecated spelling of \
                         `on_failure=\"succeed\"`; use `on_failure=\"succeed\"`"
                    )
                } else {
                    format!("`auto_status` on {what} is deprecated and has no effect unless true")
                };
                self.diags
                    .warning("deprecated.auto_status", attr.span.clone(), message);
                continue;
            }
            if key == "acp_command" {
                self.diags.unsupported(
                    "acp_command",
                    attr.span.clone(),
                    "`acp_command` is the legacy spelling",
                    "use `acp.command`",
                );
                continue;
            }
            if let Some((_, why)) = ignored.iter().find(|(ignored, _)| *ignored == key) {
                self.diags.warning(
                    &format!("ignored.{key}"),
                    attr.span.clone(),
                    format!("`{key}` on {what} is ignored: {why}"),
                );
                continue;
            }
            if known.contains(&key) || attrs::LAYOUT.contains(&key) {
                continue;
            }
            self.diags.warning(
                "fabro.unknown_attribute",
                attr.span.clone(),
                format!("`{key}` on {what} is not a Fabro attribute and is ignored"),
            );
        }
    }

    /// The `workflow.toml` path beside the root workflow, for spans.
    fn workflow_toml_path(&self) -> String {
        if self.base_dir.is_empty() {
            "workflow.toml".to_string()
        } else {
            format!("{}/workflow.toml", self.base_dir)
        }
    }

    /// Apply the resolved `[run.environment]` to the one scope: its literal
    /// `env` and, for a container provider with an image, the container
    /// target. Secret values reach commands through their configs.
    fn environment_scope(&mut self) {
        let Some(environment) = self.settings.environment.clone() else {
            return;
        };
        let scope = self
            .b
            .graph_mut()
            .body
            .scopes
            .iter_mut()
            .find(|scope| scope.id == self.scope);
        let Some(scope) = scope else {
            return;
        };
        for (key, value) in &environment.env {
            if let workflow_toml::EnvValue::Literal(text) = value {
                scope.env.insert(
                    SmolStr::new(key),
                    ir::ExprOrValue::Value(Value::String(text.clone())),
                );
            }
        }
        if let Some(image) = &environment.image
            && environment.provider != "local"
        {
            scope.runtime.target = ir::RuntimeTarget::Container {
                image:       SmolStr::new(image),
                options:     ir::ContainerOptions::default(),
                credentials: None,
            };
        }
    }

    /// The branch source nodes a prompted fan-in joins, for its prompt and
    /// its record.
    fn fan_in_sources(node: &NodeDecl, workflow: &Workflow, config: &mut Value) {
        let sources: Vec<Value> = workflow
            .incoming(&node.id)
            .into_iter()
            .map(|edge| Value::String(edge.from.clone()))
            .collect();
        if let Value::Object(config) = config {
            config.insert("sources".into(), Value::Array(sources));
        }
    }

    fn template_error(&mut self, error: &TemplateError, span: &Span, what: &str) {
        match error {
            TemplateError::Unbound { name } if self.lenient_unbound => self.diags.warning(
                "fabro.unbound_input",
                span.clone(),
                format!(
                    "{what} reads `{{{{ {name} }}}}`, which no input binds; it is left unrendered \
                     because no inputs were given. Pass `--input {}=VALUE` to render it",
                    name.strip_prefix("inputs.").unwrap_or(name)
                ),
            ),
            TemplateError::Unbound { name } => self.diags.unsupported(
                "template.unbound_input",
                span.clone(),
                format!("{what} reads `{{{{ {name} }}}}`, which no input binds"),
                &format!(
                    "pass `--input {}=VALUE`, or add a default under `[run.inputs]` in \
                     workflow.toml",
                    name.strip_prefix("inputs.").unwrap_or(name)
                ),
            ),
            other => self
                .diags
                .error("fabro.template", span.clone(), format!("{what}: {other}")),
        }
    }

    /// Render a prompt-like text, resolving a leading `@file` first. An
    /// `{% include %}` resolves beside the file it appears in. Under a
    /// lenient check an unbound input leaves the text as written.
    fn rendered(&mut self, text: &str, span: &Span, what: &str) -> Option<String> {
        self.render_text(text, span, what, true)
    }

    /// [`Self::rendered`]; `keep_unrendered` says whether a lenient check
    /// keeps the text an unbound input left unrendered, or drops it because
    /// it must parse afterwards (a `model_stylesheet`).
    fn render_text(
        &mut self,
        text: &str,
        span: &Span,
        what: &str,
        keep_unrendered: bool,
    ) -> Option<String> {
        let mut include_dir = self.base_dir.clone();
        let text = match text.strip_prefix('@') {
            Some(reference) => {
                let path = if self.base_dir.is_empty() {
                    reference.to_string()
                } else {
                    format!("{}/{reference}", self.base_dir)
                };
                if let Some(index) = path.rfind('/') {
                    include_dir = path[..index].to_string();
                }
                let Some(content) = self.files.read(&path) else {
                    self.diags.error(
                        "fabro.file_not_found",
                        span.clone(),
                        format!("{what} refers to `@{reference}`, and `{path}` cannot be read"),
                    );
                    return None;
                };
                content
            }
            None => text.to_string(),
        };
        let includes = template::Includes {
            files:    self.files,
            base_dir: include_dir,
        };
        match template::render_with(&text, &self.template, Some(includes)) {
            Ok(rendered) => Some(rendered),
            Err(error) => {
                let unrendered = keep_unrendered && self.lenient_unbound && error.is_unbound();
                self.template_error(&error, span, what);
                unrendered.then_some(text)
            }
        }
    }

    // ── Graph level ────────────────────────────────────────────────────────

    fn graph_attrs(&mut self, workflow: &mut Workflow) {
        let attrs = workflow.attrs.clone();
        self.unknown_attrs(&attrs, attrs::GRAPH, attrs::GRAPH_IGNORED, "the graph");
        let span = workflow.span.clone();
        let goal_span = attrs.span_of("goal", &span);
        // The graph's `goal` wins over `[run] goal`, as Fabro's run
        // materialization orders them.
        let goal = attrs
            .text("goal")
            .filter(|goal| !goal.trim().is_empty())
            .or_else(|| self.settings.goal.clone())
            .unwrap_or_default();
        if let Some(goal) = self.rendered(&goal, &goal_span, "the graph `goal`") {
            self.goal.clone_from(&goal);
            self.template.set_goal(goal);
        }
        match attrs.text("selection").as_deref() {
            None | Some("deterministic") => {}
            Some("random") => self.random = true,
            Some(other) => self.diags.error(
                "fabro.bad_selection",
                attrs.span_of("selection", &span),
                format!("`selection` must be `deterministic` or `random`, not `{other}`"),
            ),
        }
        if let Some(sheet) = attrs.text("model_stylesheet") {
            let sheet_span = attrs.span_of("model_stylesheet", &span);
            if let Some(rendered) =
                self.render_text(&sheet, &sheet_span, "the `model_stylesheet`", false)
            {
                match stylesheet::parse(&rendered) {
                    Ok(sheet) => stylesheet::apply(&sheet, workflow, &sheet_span, &mut self.diags),
                    Err(error) => self.diags.error(
                        "fabro.stylesheet.syntax",
                        sheet_span,
                        format!("`model_stylesheet`: {error}"),
                    ),
                }
            }
        }
        for key in ["on_failure", "on_retries_exhausted"] {
            self.check_policy(key, &attrs, &span);
        }
        threads::check_graph(workflow, &mut self.diags);
    }

    /// Diagnose one failure-policy attribute: an unknown spelling is an
    /// error, and the `succeed` shim is a dated warning.
    fn check_policy(&mut self, key: &str, attrs: &Attrs, span: &Span) {
        let Some(value) = attrs.text(key) else {
            return;
        };
        match Policy::parse(&value) {
            Some(Policy::PartiallySucceed) => self.diags.warning(
                "fabro.petri_extension",
                attrs.span_of(key, span),
                format!(
                    "`{key}=\"partially_succeed\"` is a Petri extension: Fabro's validator \
                     accepts `route`, `exit` and `succeed` only, so Fabro refuses this workflow"
                ),
            ),
            Some(_) => {}
            None => self.diags.error(
                "fabro.bad_on_failure",
                attrs.span_of(key, span),
                format!(
                    "`{key}` must be `route`, `exit`, `succeed` or `partially_succeed`, not \
                     `{value}`"
                ),
            ),
        }
    }

    // ── Structure ──────────────────────────────────────────────────────────

    /// The checks a graph must pass before lowering makes sense.
    fn structure(&mut self, workflow: &Workflow) -> Option<Structure> {
        let mut ok = true;
        for node in &workflow.nodes {
            if !node.declared {
                self.diags.error(
                    "fabro.undeclared_node",
                    node.span.clone(),
                    format!("`{}` is named by an edge but never declared", node.id),
                );
                ok = false;
            }
        }
        if workflow.node(GOAL_CHECK_NODE).is_some() {
            self.diags.error(
                "fabro.reserved_node_id",
                workflow.node(GOAL_CHECK_NODE)?.span.clone(),
                format!("`{GOAL_CHECK_NODE}` is reserved for goal-gate lowering"),
            );
            ok = false;
        }
        let starts: Vec<&NodeDecl> = workflow
            .nodes
            .iter()
            .filter(|node| {
                shape_of(node) == "Mdiamond"
                    || node.attrs.text("type").as_deref() == Some("start")
                    || matches!(node.id.as_str(), "start" | "Start")
            })
            .collect();
        let exits: Vec<&NodeDecl> = workflow
            .nodes
            .iter()
            .filter(|node| {
                shape_of(node) == "Msquare"
                    || node.attrs.text("type").as_deref() == Some("exit")
                    || matches!(node.id.as_str(), "exit" | "Exit" | "end" | "End")
            })
            .collect();
        if starts.is_empty() {
            self.diags.error(
                "fabro.no_start",
                workflow.span.clone(),
                "the workflow has no start node (`shape=Mdiamond`, `type=start`, or an id of `start`)",
            );
            return None;
        }
        if starts.len() > 1 {
            self.diags.error(
                "fabro.multiple_starts",
                workflow.span.clone(),
                format!(
                    "the workflow has multiple start nodes: {}",
                    starts
                        .iter()
                        .map(|node| node.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            ok = false;
        }
        if exits.is_empty() {
            self.diags.error(
                "fabro.no_exit",
                workflow.span.clone(),
                "the workflow has no exit node (`shape=Msquare`, `type=exit`, or an id of `exit`)",
            );
            return None;
        }
        if exits.len() > 1 {
            self.diags.error(
                "fabro.multiple_exits",
                workflow.span.clone(),
                format!(
                    "the workflow has multiple exit nodes: {}",
                    exits
                        .iter()
                        .map(|node| node.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            ok = false;
        }
        let start = starts[0].id.clone();
        let exit = exits[0].id.clone();
        for edge in &workflow.edges {
            if edge.to == start {
                self.diags.error(
                    "fabro.start_has_incoming",
                    edge.span.clone(),
                    format!(
                        "`{}` points at the start node, which takes no incoming edges",
                        edge.from
                    ),
                );
                ok = false;
            }
            if edge.from == exit {
                self.diags.error(
                    "fabro.exit_has_outgoing",
                    edge.span.clone(),
                    "the exit node has an outgoing edge; nothing runs after exit",
                );
                ok = false;
            }
        }
        // Reachability from start, over the declared edges.
        let mut seen: HashSet<&str> = HashSet::from([start.as_str()]);
        let mut queue = VecDeque::from([start.as_str()]);
        while let Some(id) = queue.pop_front() {
            for edge in workflow.outgoing(id) {
                if seen.insert(&edge.to) {
                    queue.push_back(&edge.to);
                }
            }
        }
        for node in &workflow.nodes {
            if !seen.contains(node.id.as_str()) {
                self.diags.error(
                    "fabro.unreachable_node",
                    node.span.clone(),
                    format!("`{}` is not reachable from the start node", node.id),
                );
                ok = false;
            }
        }
        if !seen.contains(exit.as_str()) {
            self.diags.error(
                "fabro.exit_unreachable",
                workflow.span.clone(),
                "the exit node is not reachable from the start node",
            );
            ok = false;
        }
        ok.then_some(Structure { start, exit })
    }

    fn kinds(&mut self, workflow: &Workflow, structure: &Structure) {
        for node in &workflow.nodes {
            let kind = if node.id == structure.start {
                Kind::Start
            } else if node.id == structure.exit {
                Kind::Exit
            } else {
                self.kind_of(node)
            };
            self.kinds.insert(node.id.clone(), kind);
        }
    }

    fn kind_of(&mut self, node: &NodeDecl) -> Kind {
        if let Some(name) = node.attrs.text("type") {
            return Kind::from_type(&name).unwrap_or_else(|| {
                self.diags.error(
                    "fabro.unknown_type",
                    node.attrs.span_of("type", &node.span),
                    format!("`type=\"{name}\"` is not a Fabro handler type"),
                );
                Kind::Agent
            });
        }
        let shape = shape_of(node);
        Kind::from_shape(&shape).unwrap_or_else(|| {
            self.diags.warning(
                "fabro.unknown_shape",
                node.attrs.span_of("shape", &node.span),
                format!("`shape={shape}` is not a Fabro shape; the node runs as an agent (`box`)"),
            );
            Kind::Agent
        })
    }

    // ── Nodes ──────────────────────────────────────────────────────────────

    fn node(&mut self, node: &NodeDecl, workflow: &Workflow) -> Resolved {
        self.unknown_attrs(
            &node.attrs,
            attrs::NODE,
            attrs::NODE_IGNORED,
            &format!("node `{}`", node.id),
        );
        let kind = self.kinds[&node.id];
        let id = self.ids[&node.id];
        let shape = shape_of(node);
        let label = node.attrs.text("label").unwrap_or_else(|| node.id.clone());
        for key in ["on_failure", "on_retries_exhausted"] {
            self.check_policy(key, &node.attrs, &node.span);
        }
        let policy = FailurePolicy::of(node, workflow, &mut self.diags);
        let explicit = self.explicit_timeout(node);

        let mut meta = json!({
            "label": label,
            "shape": shape,
            "kind": kind.name(),
            "classes": node.classes,
            "span": { "line": node.span.line, "column": node.span.column },
        });
        for key in ["model", "provider", "reasoning_effort"] {
            if let Some(value) = node.attrs.text(key) {
                meta[key] = Value::String(value);
            }
        }
        self.b.set_meta(id, meta);

        let (step, timeout) = match kind {
            Kind::Conditional => (None, STRUCTURAL_TIMEOUT),
            // `start` and `exit` run the stage step: it records the scope's
            // environment for sandbox-placed hooks and fires the run-level
            // hooks (`sandbox_ready`, `run_start`, `run_complete`).
            Kind::Start | Kind::Exit => {
                let kv = self.b.exprs().var("kv");
                let mut config = json!({
                    "node": node.id,
                    "kind": kind.name(),
                    "label": label,
                    "workflow": self.workflow_name,
                    "kv": placeholder(kv),
                });
                if self.stack.len() == 1 {
                    config["hooks"] = hooks::param(&self.hooks);
                }
                (Some(StepRef::new(STAGE_KIND, config)), STRUCTURAL_TIMEOUT)
            }
            Kind::FanIn => {
                if node
                    .attrs
                    .text("prompt")
                    .is_some_and(|prompt| !prompt.trim().is_empty())
                {
                    // A prompted fan-in: the ordered barrier, then one model
                    // call over the branch results.
                    let mut config = self.agent_config(node, kind, workflow, policy, explicit);
                    Self::fan_in_sources(node, workflow, &mut config);
                    (
                        Some(StepRef::new(PROMPT_KIND, config)),
                        explicit.unwrap_or(AGENT_TIMEOUT),
                    )
                } else {
                    (None, STRUCTURAL_TIMEOUT)
                }
            }
            Kind::Parallel => (None, explicit.unwrap_or(STRUCTURAL_TIMEOUT)),
            Kind::Agent => {
                let mut config = self.agent_config(node, kind, workflow, policy, explicit);
                if let Some(object) = config.as_object_mut() {
                    object.insert("compaction".into(), self.settings.compaction.to_json());
                }
                (
                    Some(StepRef::new(AGENT_KIND, config)),
                    explicit.unwrap_or(AGENT_TIMEOUT),
                )
            }
            Kind::Prompt => {
                let config = self.agent_config(node, kind, workflow, policy, explicit);
                (
                    Some(StepRef::new(PROMPT_KIND, config)),
                    explicit.unwrap_or(AGENT_TIMEOUT),
                )
            }
            Kind::Command => {
                let config = self.command_config(node, workflow, policy, explicit);
                (
                    Some(StepRef::new(COMMAND_KIND, config)),
                    explicit.unwrap_or(COMMAND_TIMEOUT),
                )
            }
            Kind::Human => {
                let config = self.human_config(node, workflow, policy, explicit);
                (
                    Some(StepRef::new(HUMAN_KIND, config)),
                    explicit.unwrap_or(HUMAN_TIMEOUT),
                )
            }
            Kind::Wait => {
                let duration = node.attrs.duration("duration", &mut self.diags);
                if duration.is_none() && !node.attrs.contains("duration") {
                    self.diags.error(
                        "fabro.wait_requires_duration",
                        node.span.clone(),
                        format!("wait node `{}` needs a `duration`", node.id),
                    );
                }
                let duration = duration.unwrap_or_default();
                let config = json!({ "label": label, "duration_ms": duration_ms(duration) });
                let timeout = explicit.unwrap_or(duration + STRUCTURAL_TIMEOUT);
                (Some(StepRef::new(WAIT_KIND, config)), timeout)
            }
            Kind::ManagerLoop => {
                let config = self.workflow_config(node, policy);
                (
                    Some(StepRef::new(WORKFLOW_KIND, config)),
                    explicit.unwrap_or(AGENT_TIMEOUT),
                )
            }
        };
        if let Some(step) = step {
            self.b.node_mut(id).step = step;
        }
        self.b.node_mut(id).budget = Budget::new(1, timeout)
            .with_timeout_policy(policy::timeout_policy(kind, node, workflow));
        self.b.node_mut(id).retry = routing::retry_policy(node, workflow, policy, &mut self.diags);
        Resolved { kind, id, policy }
    }

    /// The node's `timeout`. A bare number is Attractor's spelling (seconds);
    /// Fabro needs a unit, and reads a unitless value as no timeout at all.
    fn explicit_timeout(&mut self, node: &NodeDecl) -> Option<Duration> {
        if let Some(attr) = node.attrs.get("timeout")
            && attr.value.as_text().trim().parse::<f64>().is_ok()
        {
            self.diags.unsupported(
                "attractor",
                attr.span.clone(),
                format!(
                    "`timeout={}` is a bare number, the Attractor spelling",
                    attr.value.as_text()
                ),
                "write the unit: `timeout=\"1200s\"`",
            );
            return None;
        }
        node.attrs.duration("timeout", &mut self.diags)
    }

    /// The shared part of every step config: label, goal, the run context.
    fn base_config(
        &mut self,
        node: &NodeDecl,
        workflow: &Workflow,
        policy: FailurePolicy,
        timeout: Option<Duration>,
    ) -> Map<String, Value> {
        let mut config = Map::new();
        config.insert(
            "label".into(),
            Value::String(node.attrs.text("label").unwrap_or_else(|| node.id.clone())),
        );
        config.insert("node".into(), Value::String(node.id.clone()));
        config.insert("goal".into(), Value::String(self.goal.clone()));
        let kv = self.b.exprs().var("kv");
        config.insert("kv".into(), placeholder(kv));
        config.insert(
            "on_failure".into(),
            Value::String(policy.on_failure.name().into()),
        );
        if let Some(ms) = timeout.map(duration_ms) {
            config.insert("timeout_ms".into(), ms);
        }
        if matches!(
            policy.on_failure,
            Policy::Succeed | Policy::PartiallySucceed
        ) {
            // Fabro promotes a failure only when no explicit route matches
            // it; the step decides that with the node's routes in hand.
            config.insert(
                ROUTES_KEY.into(),
                promotion::explicit_routes(node, workflow),
            );
        }
        config
    }

    fn agent_config(
        &mut self,
        node: &NodeDecl,
        kind: Kind,
        workflow: &Workflow,
        policy: FailurePolicy,
        timeout: Option<Duration>,
    ) -> Value {
        let mut config = self.base_config(node, workflow, policy, timeout);
        config.insert("kind".into(), Value::String(kind.name().into()));
        let prompt_span = node.attrs.span_of("prompt", &node.span);
        let prompt = match node.attrs.text("prompt") {
            Some(prompt) if !prompt.trim().is_empty() => prompt,
            _ => {
                self.diags.warning(
                    "fabro.prompt_missing",
                    node.span.clone(),
                    format!(
                        "agent node `{}` has no `prompt`; its label is the prompt",
                        node.id
                    ),
                );
                node.attrs.text("label").unwrap_or_else(|| node.id.clone())
            }
        };
        if let Some(prompt) = self.rendered(
            &prompt,
            &prompt_span,
            &format!("node `{}` `prompt`", node.id),
        ) {
            config.insert("prompt".into(), Value::String(prompt));
        }
        let is_prompt = kind != Kind::Agent;
        if let Some(backend) = node
            .attrs
            .text("backend")
            .or_else(|| workflow.attrs.text("backend"))
        {
            if !matches!(backend.as_str(), "acp" | "api") {
                self.diags.error(
                    "fabro.bad_backend",
                    node.attrs.span_of("backend", &node.span),
                    "agent backend must be acp or api",
                );
            }
            // Fabro's `backend_valid` rule: a prompt node is API-only. The
            // graph's `backend="acp"` applies to agent nodes only.
            if is_prompt && backend == "acp" {
                if node.attrs.contains("backend") {
                    self.diags.error(
                        "fabro.prompt_backend",
                        node.attrs.span_of("backend", &node.span),
                        "backend=\"acp\" is only valid on agent nodes; prompt nodes are API-only",
                    );
                }
            } else {
                config.insert("backend".into(), Value::String(backend));
            }
        }
        for key in ["model", "provider", "reasoning_effort"] {
            if let Some(value) = node.attrs.text(key) {
                config.insert(key.into(), Value::String(value));
            }
        }
        // The graph's defaults, then `[run.model]` from `workflow.toml`.
        if !config.contains_key("model")
            && let Some(model) = workflow
                .attrs
                .text("default_model")
                .or_else(|| self.settings.model.name.clone())
        {
            config.insert("model".into(), Value::String(model));
        }
        if !config.contains_key("provider")
            && let Some(provider) = workflow
                .attrs
                .text("default_provider")
                .or_else(|| self.settings.model.provider.clone())
        {
            config.insert("provider".into(), Value::String(provider));
        }
        if !config.contains_key("reasoning_effort")
            && let Some(effort) = self.settings.model.reasoning_effort.clone()
        {
            config.insert("reasoning_effort".into(), Value::String(effort));
        }
        let branch_first = threads::is_branch_first(node, workflow, &self.kinds);
        let default_speed = self.settings.model.speed.clone();
        let threads = threads::ThreadAttrs::read(
            node,
            workflow,
            branch_first,
            default_speed.as_deref(),
            &mut self.diags,
        );
        threads.write(self.b.exprs(), &mut config);
        config.insert("stages".into(), threads::stages(workflow, &self.kinds));
        self.output_schema(node, &mut config);
        if let Some(retries) = node.attrs.int("output_retries", &mut self.diags) {
            let retries = retries.max(0);
            if retries > i64::try_from(MAX_OUTPUT_RETRIES).unwrap_or(i64::MAX) {
                self.diags.error(
                    "fabro.output_retries_too_large",
                    node.attrs.span_of("output_retries", &node.span),
                    format!("`output_retries={retries}` exceeds the hard maximum of {MAX_OUTPUT_RETRIES}"),
                );
            }
            config.insert(
                "output_retries".into(),
                Value::from(retries.min(i64::try_from(MAX_OUTPUT_RETRIES).unwrap_or(i64::MAX))),
            );
        }
        if is_prompt {
            if node.attrs.text("acp.command").is_some() || node.attrs.text("acp.config").is_some() {
                self.diags.error(
                    "fabro.backend_options",
                    node.span.clone(),
                    "a prompt node cannot set acp.command or acp.config; prompt nodes are API-only",
                );
            }
        } else if config.get("backend").and_then(Value::as_str) == Some("api") {
            if node.attrs.text("acp.command").is_some() || node.attrs.text("acp.config").is_some() {
                self.diags.error(
                    "fabro.backend_options",
                    node.span.clone(),
                    "an API node cannot set acp.command or acp.config",
                );
            }
        } else {
            self.acp(node, workflow, &mut config);
        }
        let nodes = self.b.exprs().var("nodes");
        config.insert("nodes".into(), placeholder(nodes));
        Value::Object(config)
    }

    fn output_schema(&mut self, node: &NodeDecl, config: &mut Map<String, Value>) {
        let Some(schema) = node.attrs.text("output_schema") else {
            return;
        };
        let span = node.attrs.span_of("output_schema", &node.span);
        if schema == "routing" {
            config.insert("output_schema".into(), Value::String("routing".into()));
            return;
        }
        let Some(text) = self.rendered(
            &schema,
            &span,
            &format!("node `{}` `output_schema`", node.id),
        ) else {
            return;
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(value @ Value::Object(_)) => {
                config.insert("output_schema".into(), value);
            }
            _ => self.diags.error(
                "fabro.bad_output_schema",
                span,
                format!(
                    "`output_schema` on `{}` must be `routing` or a JSON Schema object",
                    node.id
                ),
            ),
        }
    }

    fn acp(&mut self, node: &NodeDecl, workflow: &Workflow, config: &mut Map<String, Value>) {
        let command = node
            .attrs
            .text("acp.command")
            .or_else(|| workflow.attrs.text("acp.command"));
        let acp_config = node
            .attrs
            .text("acp.config")
            .or_else(|| workflow.attrs.text("acp.config"));
        match (command, acp_config) {
            (Some(_), Some(_)) => self.diags.error(
                "fabro.acp_both",
                node.attrs.span_of("acp.command", &node.span),
                format!(
                    "node `{}` sets both `acp.command` and `acp.config`",
                    node.id
                ),
            ),
            (Some(command), None) => {
                config.insert("acp".into(), json!({ "command": command }));
            }
            (None, Some(text)) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    config.insert("acp".into(), json!({ "config": value }));
                }
                Err(error) => self.diags.error(
                    "fabro.bad_acp_config",
                    node.attrs.span_of("acp.config", &node.span),
                    format!("`acp.config` is not JSON: {error}"),
                ),
            },
            (None, None) => {}
        }
    }

    fn command_config(
        &mut self,
        node: &NodeDecl,
        workflow: &Workflow,
        policy: FailurePolicy,
        timeout: Option<Duration>,
    ) -> Value {
        let mut config = self.base_config(node, workflow, policy, timeout);
        let language = node
            .attrs
            .text("language")
            .unwrap_or_else(|| "shell".into());
        if language != "shell" && language != "python" {
            self.diags.error(
                "fabro.bad_language",
                node.attrs.span_of("language", &node.span),
                format!("`language` must be `shell` or `python`, not `{language}`"),
            );
        }
        config.insert("language".into(), Value::String(language.clone()));
        match node.attrs.text("script") {
            Some(script) if !script.trim().is_empty() => {
                match template::render_script(&script, &language, &self.template) {
                    Ok(script) => {
                        config.insert("script".into(), Value::String(script));
                    }
                    Err(error) => {
                        let span = node.attrs.span_of("script", &node.span);
                        let unrendered = self.lenient_unbound && error.is_unbound();
                        self.template_error(&error, &span, &format!("node `{}` `script`", node.id));
                        if unrendered {
                            config.insert("script".into(), Value::String(script));
                        }
                    }
                }
            }
            _ => self.diags.error(
                "fabro.command_requires_script",
                node.span.clone(),
                format!("command node `{}` needs a `script`", node.id),
            ),
        }
        let mut env = Map::new();
        if let Some(environment) = &self.settings.environment {
            for (key, value) in &environment.env {
                if let workflow_toml::EnvValue::Secret(_) = value {
                    env.insert(key.clone(), value.to_json());
                }
            }
        }
        if let Some(prepare_env) = self.prepare_envs.get(&node.id) {
            for (key, value) in prepare_env {
                env.insert(key.clone(), value.to_json());
            }
        }
        if !env.is_empty() {
            config.insert("env".into(), Value::Object(env));
        }
        if let Some(source) = node.attrs.text("stdin_source") {
            let span = node.attrs.span_of("stdin_source", &node.span);
            match self.context_source(&source, node, workflow, &span) {
                Some(expr) => {
                    config.insert("stdin".into(), placeholder(expr));
                }
                None => self.diags.error(
                    "fabro.bad_stdin_source",
                    span,
                    format!("`stdin_source` must name a context key such as `context.output`, not `{source}`"),
                ),
            }
        }
        self.output_schema(node, &mut config);
        Value::Object(config)
    }

    /// `context.K` as an expression. `context.parallel.results` reads the
    /// nearest upstream fan-in's output, where the branch results live under
    /// the engine (they ride tokens, never `kv`).
    fn context_source(
        &mut self,
        source: &str,
        node: &NodeDecl,
        workflow: &Workflow,
        span: &Span,
    ) -> Option<ExprId> {
        let key = source.strip_prefix("context.").unwrap_or(source).trim();
        if key.is_empty() {
            return None;
        }
        if key == "parallel.results" && !self.upstream_fan_in(&node.id, workflow) {
            self.diags.warning(
                "fabro.parallel_results_without_fan_in",
                span.clone(),
                format!(
                    "`{}` reads `context.parallel.results` but no fan-in node precedes it",
                    node.id
                ),
            );
        }
        if key.starts_with("internal.") {
            self.diags.warning(
                "fabro.internal_context",
                span.clone(),
                format!("`{key}` is Fabro-internal run state, which Petri does not populate; it reads as null"),
            );
        }
        let kv = self.b.exprs().var("kv");
        let name = self.b.exprs().lit(key);
        Some(self.b.exprs().call("get", vec![kv, name]))
    }

    /// Whether a fan-in, or a parallel node whose fan-in publishes the
    /// results, precedes `id`.
    fn upstream_fan_in(&self, id: &str, workflow: &Workflow) -> bool {
        let mut seen: HashSet<&str> = HashSet::from([id]);
        let mut queue: VecDeque<&str> = VecDeque::from([id]);
        while let Some(current) = queue.pop_front() {
            for edge in workflow.incoming(current) {
                if matches!(
                    self.kinds.get(&edge.from),
                    Some(Kind::FanIn | Kind::Parallel)
                ) {
                    return true;
                }
                if seen.insert(&edge.from) {
                    queue.push_back(&edge.from);
                }
            }
        }
        false
    }

    fn human_config(
        &mut self,
        node: &NodeDecl,
        workflow: &Workflow,
        policy: FailurePolicy,
        timeout: Option<Duration>,
    ) -> Value {
        let mut config = self.base_config(node, workflow, policy, timeout);
        let mut choices = Vec::new();
        let mut freeform_target = None;
        for edge in workflow.outgoing(&node.id) {
            if edge
                .attrs
                .bool("freeform", &mut self.diags)
                .unwrap_or(false)
            {
                if freeform_target.is_some() {
                    self.diags.error(
                        "fabro.freeform_edge_count",
                        edge.span.clone(),
                        format!(
                            "human gate `{}` has more than one `freeform=true` edge",
                            node.id
                        ),
                    );
                }
                freeform_target = Some(edge.to.clone());
                continue;
            }
            let label = edge
                .attrs
                .text("label")
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| edge.to.clone());
            choices.push(json!({
                "key": labels::accelerator_key(&label),
                "label": label,
                "to": edge.to,
            }));
        }
        if choices.is_empty() && freeform_target.is_none() {
            self.diags.error(
                "fabro.human_without_edges",
                node.span.clone(),
                format!(
                    "human gate `{}` has no outgoing edges to offer as choices",
                    node.id
                ),
            );
        }
        if let Some(kind) = node.attrs.text("question_type") {
            if !attrs::QUESTION_TYPES.contains(&kind.as_str()) {
                self.diags.error(
                    "fabro.bad_question_type",
                    node.attrs.span_of("question_type", &node.span),
                    format!(
                        "`question_type` must be one of {}",
                        attrs::QUESTION_TYPES.join(", ")
                    ),
                );
            }
            config.insert("question_type".into(), Value::String(kind));
        }
        if let Some(sensitive) = node.attrs.bool("sensitive", &mut self.diags) {
            config.insert("sensitive".into(), Value::Bool(sensitive));
        }
        if let Some(review) = node.attrs.bool("review_target", &mut self.diags) {
            config.insert("review_target".into(), Value::Bool(review));
        }
        if let Some(default) = node.attrs.text("human.default_choice") {
            if !choices.iter().any(|choice| {
                choice["key"] == Value::String(default.clone())
                    || choice["to"] == Value::String(default.clone())
            }) {
                self.diags.error(
                    "fabro.bad_default_choice",
                    node.attrs.span_of("human.default_choice", &node.span),
                    format!(
                        "`human.default_choice=\"{default}\"` names none of the gate's \
                         choices or targets"
                    ),
                );
            }
            config.insert("default_choice".into(), Value::String(default));
        }
        config.insert("choices".into(), Value::Array(choices));
        if let Some(target) = freeform_target {
            config.insert("freeform_target".into(), Value::String(target));
        }
        Value::Object(config)
    }

    fn workflow_config(&mut self, node: &NodeDecl, policy: FailurePolicy) -> Value {
        let mut config = Map::new();
        config.insert(
            "label".into(),
            Value::String(node.attrs.text("label").unwrap_or_else(|| node.id.clone())),
        );
        config.insert("node".into(), Value::String(node.id.clone()));
        config.insert(
            "on_failure".into(),
            Value::String(policy.on_failure.name().into()),
        );
        let kv = self.b.exprs().var("kv");
        config.insert("kv".into(), placeholder(kv));
        // Fabro's normalization: missing, non-integer or negative is 1000;
        // zero is 1. A value Fabro would silently discard is named here.
        let cycles = match node.attrs.get("manager.max_cycles") {
            None => 1000,
            Some(attr) => match &attr.value {
                model::AttrValue::Int(n) if *n >= 0 => (*n).max(1),
                model::AttrValue::Str(s) if s.trim().parse::<i64>().is_ok_and(|n| n >= 0) => {
                    s.trim().parse::<i64>().unwrap_or(1000).max(1)
                }
                other => {
                    self.diags.warning(
                        "fabro.manager.max_cycles",
                        attr.span.clone(),
                        format!(
                            "`manager.max_cycles={}` is not a non-negative integer; Fabro reads \
                             it as 1000",
                            other.as_text()
                        ),
                    );
                    1000
                }
            },
        };
        config.insert("max_cycles".into(), Value::from(cycles));
        if let Some(interval) = node
            .attrs
            .duration("manager.poll_interval", &mut self.diags)
        {
            config.insert("poll_interval_ms".into(), duration_ms(interval));
        }
        if let Some(stop) = node.attrs.text("manager.stop_condition") {
            let span = node.attrs.span_of("manager.stop_condition", &node.span);
            let mut table = ir::ExprTable::new();
            if condition::lower(&stop, &mut table, &span, &mut self.diags, false).is_some() {
                config.insert("stop_condition".into(), Value::String(stop));
            }
        }
        let source = node.attrs.text("stack.child_workflow");
        let inline = node.attrs.text("stack.child_dot_source");
        let child = match (source, inline) {
            (Some(path), _) => {
                config.insert("child_workflow".into(), Value::String(path.clone()));
                let span = node.attrs.span_of("stack.child_workflow", &node.span);
                self.child_from_file(&path, &span, &node.id)
            }
            (None, Some(source)) => {
                config.insert("child_dot_source".into(), Value::String(source.clone()));
                let span = node.attrs.span_of("stack.child_dot_source", &node.span);
                let name = format!(
                    "{}#{}",
                    self.stack.last().map_or("", String::as_str),
                    node.id
                );
                self.child_from_text(&name, &source, &span)
            }
            (None, None) => {
                self.diags.error(
                    "fabro.manager_loop_without_child",
                    node.span.clone(),
                    format!(
                        "manager loop `{}` needs `stack.child_workflow` or `stack.child_dot_source`",
                        node.id
                    ),
                );
                None
            }
        };
        if let Some(digest) = child {
            config.insert("child_digest".into(), Value::String(digest));
        }
        Value::Object(config)
    }

    /// Where a `stack.child_workflow` path reads from: as written against the
    /// repository root, with Fabro's bundle prefix `fabro/` standing for
    /// `.fabro/`, or beside the workflow file.
    fn child_from_file(&mut self, path: &str, span: &Span, node: &str) -> Option<String> {
        let mut candidates = vec![path.to_string()];
        if let Some(rest) = path.strip_prefix("fabro/") {
            candidates.push(format!(".fabro/{rest}"));
        }
        if !self.base_dir.is_empty() {
            candidates.push(format!("{}/{path}", self.base_dir));
        }
        let found = candidates.iter().find_map(|candidate| {
            self.files
                .read(candidate)
                .map(|text| (candidate.clone(), text))
        });
        let Some((resolved, text)) = found else {
            self.diags.error(
                "fabro.child_workflow_not_found",
                span.clone(),
                format!(
                    "manager loop `{node}` names `{path}`, which cannot be read ({} tried)",
                    candidates.join(", ")
                ),
            );
            return None;
        };
        self.child_from_text(&resolved, &text, span)
    }

    /// Lower a child workflow now, so `petri check` validates it and the run
    /// registers it before the root starts. Its digest names it.
    fn child_from_text(&mut self, name: &str, text: &str, span: &Span) -> Option<String> {
        if self.stack.iter().any(|f| f == name) {
            self.diags.error(
                "fabro.workflow_cycle",
                span.clone(),
                format!(
                    "workflow call cycle: {} -> `{name}`",
                    self.stack.join(" -> ")
                ),
            );
            return None;
        }
        if self.stack.len() > MAX_CALL_DEPTH {
            self.diags.error(
                "fabro.workflow_depth",
                span.clone(),
                format!("nested workflows nest more than {MAX_CALL_DEPTH} deep at `{name}`"),
            );
            return None;
        }
        let dot = match dot::parse(name, text) {
            Ok(dot) => dot,
            Err(diagnostic) => {
                self.diags.push(diagnostic);
                return None;
            }
        };
        let workflow = model::build(&dot);
        let to_map = |map: &BTreeMap<String, Value>| {
            map.iter()
                .map(|(k, v)| (SmolStr::new(k), v.clone()))
                .collect()
        };
        let inputs = CompileInputs {
            inputs:             to_map(self.template.inputs()),
            vars:               to_map(self.template.vars()),
            unbound_is_warning: self.lenient_unbound,
        };
        let lowered = lower_nested(
            workflow,
            name,
            self.files,
            &inputs,
            Diagnostics::new(),
            self.stack.clone(),
        );
        for diagnostic in lowered.diagnostics.iter() {
            self.diags.push(diagnostic.clone());
        }
        let graph = lowered.graph?;
        let digest = frontend::graph_digest(&graph);
        self.children.extend(lowered.children);
        self.children.push(graph);
        Some(digest)
    }

    // ── Goal gates ─────────────────────────────────────────────────────────

    /// The `goal_check` node in front of `exit`, when any node is a goal
    /// gate: every gate must have a success-like record, or the run jumps
    /// back to the gate's retry target — the first that exists of the node's
    /// `retry_target`, its `fallback_retry_target`, the graph's, and the
    /// graph's fallback — and a gate with no target ends the run failed.
    fn goal_check(&mut self, workflow: &Workflow, exit: NodeId) -> Option<NodeId> {
        let mut gates: Vec<&NodeDecl> = Vec::new();
        for node in &workflow.nodes {
            if node
                .attrs
                .bool("goal_gate", &mut self.diags)
                .unwrap_or(false)
            {
                gates.push(node);
            }
        }
        if gates.is_empty() {
            return None;
        }
        gates.sort_by(|a, b| a.id.cmp(&b.id));
        let exit_span = self.spans[&exit].clone();
        let check = self.b.add_node(
            GOAL_CHECK_NODE,
            self.scope,
            StepRef::new("noop", Value::Null),
        );
        self.spans.insert(check, exit_span.clone());
        self.b.set_meta(
            check,
            json!({
                "label": "Goal check",
                "shape": "diamond",
                "kind": "goal_check",
                "classes": [],
                "synthetic": true,
            }),
        );

        let mut arms = Vec::new();
        let mut all_ok: Option<ExprId> = None;
        for gate in &gates {
            let ok = {
                let record = self.b.exprs().path("nodes", &[&gate.id, "success_like"]);
                let falsy = self.b.exprs().lit(false);
                self.b.exprs().call("default", vec![record, falsy])
            };
            all_ok = Some(match all_ok {
                None => ok,
                Some(acc) => self.b.exprs().binary(ir::BinOp::And, acc, ok),
            });
            let failing = self.b.exprs().unary(ir::UnOp::Not, ok);
            let target = [
                gate.attrs.text("retry_target"),
                gate.attrs.text("fallback_retry_target"),
                workflow.attrs.text("retry_target"),
                workflow.attrs.text("fallback_retry_target"),
            ]
            .into_iter()
            .flatten()
            .find(|t| self.ids.contains_key(t));
            match target {
                Some(target) => {
                    let id = self.b.next_edge_id();
                    let mut edge = Edge::when(id, self.ids[&target], failing);
                    edge.back = true;
                    edge.label = Some(SmolStr::new(format!("goal_gate:{}", gate.id)));
                    arms.push(edge);
                }
                None => self.diags.warning(
                    "fabro.goal_gate_without_target",
                    gate.span.clone(),
                    format!(
                        "goal gate `{}` has no retry target that exists; when it fails the run ends failed",
                        gate.id
                    ),
                ),
            }
        }
        let all_ok = all_ok.expect("at least one gate");
        let id = self.b.next_edge_id();
        arms.push(Edge::when(id, exit, all_ok));
        self.b.node_mut(check).routing = Routing::select(arms);
        Some(check)
    }

    // ── Routing ────────────────────────────────────────────────────────────

    fn routing(
        &mut self,
        node: &NodeDecl,
        res: &Resolved,
        workflow: &Workflow,
        exit: NodeId,
        goal_check: Option<NodeId>,
    ) {
        if matches!(res.kind, Kind::Exit | Kind::Parallel) {
            // The exit routes nowhere; a parallel node's edges are its fan-out.
            return;
        }
        let edges = workflow.outgoing(&node.id);
        if edges.is_empty() {
            return;
        }
        let random = match node.attrs.text("selection").as_deref() {
            None => self.random,
            Some("random") => true,
            Some("deterministic") => false,
            Some(other) => {
                self.diags.error(
                    "fabro.bad_selection",
                    node.attrs.span_of("selection", &node.span),
                    format!("`selection` must be `deterministic` or `random`, not `{other}`"),
                );
                false
            }
        };
        // A `start` that fails (a blocking `run_start` hook) ends the run, as
        // Fabro's blocked run does; it never routes on.
        let policy = if res.kind == Kind::Start {
            FailurePolicy {
                on_failure:           Policy::Exit,
                on_retries_exhausted: Policy::Exit,
            }
        } else {
            res.policy
        };
        let mut lowered = Vec::with_capacity(edges.len());
        for edge in &edges {
            let Some(mut to) = self.ids.get(&edge.to).copied() else {
                continue;
            };
            if let Some(check) = goal_check
                && to == exit
            {
                to = check;
            }
            self.unknown_attrs(
                &edge.attrs,
                attrs::EDGE,
                &[],
                &format!("edge `{} -> {}`", edge.from, edge.to),
            );
            let cond = self.edge_condition(edge, policy);
            let weight = edge.attrs.int("weight", &mut self.diags).unwrap_or(0);
            let label = edge.attrs.text("label").filter(|l| !l.is_empty());
            let restart = edge
                .attrs
                .bool("loop_restart", &mut self.diags)
                .unwrap_or(false);
            let graph_full = workflow.attrs.text("default_fidelity").as_deref() == Some("full");
            let map = threads::edge_payload(
                self.b.exprs(),
                edge,
                &self.kinds,
                graph_full,
                &mut self.diags,
            );
            lowered.push(routing::OutEdge {
                to,
                target: edge.to.clone(),
                condition: cond,
                label_key: label.as_deref().map(labels::routing_key),
                label,
                weight,
                restart,
                map,
            });
        }
        if lowered.is_empty() {
            return;
        }
        if random && lowered.iter().any(|e| e.condition.is_some()) {
            self.diags.error(
                "fabro.random_with_conditions",
                node.span.clone(),
                format!(
                    "`{}` uses `selection=\"random\"` and has conditional edges; Fabro forbids the combination",
                    node.id
                ),
            );
        }
        let group = routing::group(
            &mut self.b,
            &lowered,
            policy,
            res.kind == Kind::Human,
            random,
        );
        // The edge table hooks read: arm id to target node id and label, so
        // an `edge_selected` hook names Fabro nodes, never engine ids.
        let mut edges = Map::new();
        for (arm, out) in group.arms.iter().zip(&lowered) {
            edges.insert(
                arm.id.raw().to_string(),
                json!({ "to": out.target, "label": out.label }),
            );
        }
        let meta = &mut self.b.node_mut(res.id).meta;
        if let Value::Object(map) = meta {
            map.insert("edges".into(), Value::Object(edges));
        }
        self.b.node_mut(res.id).routing = Routing::groups(vec![group]);
    }

    fn edge_condition(&mut self, edge: &EdgeDecl, policy: FailurePolicy) -> Option<ExprId> {
        let text = edge.attrs.text("condition")?;
        if text.trim().is_empty() {
            return None;
        }
        let span = edge.attrs.span_of("condition", &edge.span);
        condition::lower(
            &text,
            self.b.exprs(),
            &span,
            &mut self.diags,
            policy.succeeds(),
        )
    }

    // ── Back edges, joins, budgets ─────────────────────────────────────────

    /// A depth-first search from `start` over the lowered edges marks every
    /// cycle-closing edge `back`.
    fn back_edges(&mut self, start: NodeId) {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        struct Frame {
            node:  NodeId,
            next:  usize,
            edges: Vec<(EdgeId, NodeId)>,
        }
        let count = self.b.graph().nodes.len();
        let mut color = vec![Color::White; count];
        let mut back: HashSet<EdgeId> = HashSet::new();
        let successors = |graph: &ir::Graph, node: NodeId| -> Vec<(EdgeId, NodeId)> {
            graph
                .node(node)
                .map(|n| n.routing.edges().map(|e| (e.id, e.to)).collect())
                .unwrap_or_default()
        };
        let mut stack = vec![Frame {
            node:  start,
            next:  0,
            edges: successors(self.b.graph(), start),
        }];
        color[start.index()] = Color::Gray;
        while let Some(frame) = stack.last_mut() {
            if frame.next >= frame.edges.len() {
                color[frame.node.index()] = Color::Black;
                stack.pop();
                continue;
            }
            let (edge, to) = frame.edges[frame.next];
            frame.next += 1;
            match color[to.index()] {
                Color::Gray => {
                    back.insert(edge);
                }
                Color::White => {
                    color[to.index()] = Color::Gray;
                    stack.push(Frame {
                        node:  to,
                        next:  0,
                        edges: successors(self.b.graph(), to),
                    });
                }
                Color::Black => {}
            }
        }
        for node in &mut self.b.graph_mut().body.nodes {
            for group in &mut node.routing.groups {
                for arm in &mut group.arms {
                    if back.contains(&arm.id) {
                        arm.back = true;
                    }
                }
            }
        }
    }

    fn joins_and_budgets(
        &mut self,
        workflow: &Workflow,
        resolved: &[Resolved],
        goal_check: Option<NodeId>,
    ) {
        let looped = loop_reachable(&self.b.graph().body);
        let global = workflow
            .attrs
            .int("max_node_visits", &mut self.diags)
            .filter(|n| *n > 0);
        if let Some(limit) = global
            && limit > i64::from(MAX_FIRINGS)
        {
            self.diags.error(
                "fabro.max_visits_too_large",
                workflow.attrs.span_of("max_node_visits", &workflow.span),
                format!("`max_node_visits={limit}` exceeds the hard maximum of {MAX_FIRINGS}"),
            );
        }
        for (node, res) in workflow.nodes.iter().zip(resolved) {
            let join = if res.kind == Kind::FanIn {
                JoinPolicy::All
            } else {
                JoinPolicy::Any
            };
            self.b.set_join(res.id, join);
            let visits = node
                .attrs
                .int("max_visits", &mut self.diags)
                .filter(|n| *n > 0);
            if let Some(limit) = visits
                && limit > i64::from(MAX_FIRINGS)
            {
                self.diags.error(
                    "fabro.max_visits_too_large",
                    node.attrs.span_of("max_visits", &node.span),
                    format!("`max_visits={limit}` exceeds the hard maximum of {MAX_FIRINGS}"),
                );
            }
            if !looped.contains(&res.id) {
                continue;
            }
            let explicit = [visits, global]
                .into_iter()
                .flatten()
                .filter_map(|n| u32::try_from(n).ok())
                .min();
            let max_firings = explicit.map_or(MAX_FIRINGS, |n| n.min(MAX_FIRINGS));
            if explicit.is_none() && !self.budget_defaulted {
                self.budget_defaulted = true;
                self.diags.warning(
                    "info.budget.default",
                    node.span.clone(),
                    format!(
                        "`{}` is in a loop with no `max_visits`; Fabro's unlimited visits lower to \
                         the hard maximum of {MAX_FIRINGS} firings",
                        node.id
                    ),
                );
            }
            let budget = self
                .b
                .graph()
                .node(res.id)
                .map_or_else(|| Budget::new(1, STRUCTURAL_TIMEOUT), |n| n.budget);
            self.b.set_budget(res.id, Budget {
                max_firings,
                ..budget
            });
        }
        // The synthetic goal check, when it exists, loops too.
        if let Some(check) = goal_check {
            self.b.set_join(check, JoinPolicy::Any);
            if looped.contains(&check) {
                let limit = global
                    .and_then(|n| u32::try_from(n).ok())
                    .map_or(MAX_FIRINGS, |n| n.min(MAX_FIRINGS));
                self.b
                    .set_budget(check, Budget::new(limit, STRUCTURAL_TIMEOUT));
            }
        }
    }
}
