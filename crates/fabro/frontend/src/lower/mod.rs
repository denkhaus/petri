//! Fabro workflow → HIR.
//!
//! One pass over the semantic [`Workflow`]: node kinds and step configs,
//! then routing — one tiered group per node, the four Fabro tiers — then the
//! implicit edges (goal gates), back-edge classification, joins and budgets,
//! and finally the engine's own validation mapped back onto source spans.
//! Every construct here lowers onto what the core has; nothing invents engine
//! semantics.

mod attrs;
mod routing;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use frontend::{CompileInputs, Diagnostics, FileSource, Lowered, Span};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::validate::loop_reachable;
use ir::{
    Budget, Completion, Edge, EdgeId, ExpandTarget, ExprId, GraphBuilder, JoinPolicy, NodeId,
    Routing, Scope, ScopeId, StepRef,
};
pub use routing::{FailurePolicy, Policy};
use serde_json::{Map, Value, json};
use smol_str::SmolStr;

use crate::kinds::{AGENT_KIND, COMMAND_KIND, HUMAN_KIND, WAIT_KIND, WORKFLOW_KIND};
use crate::model::{Attrs, EdgeDecl, NodeDecl, Workflow};
use crate::template::{self, Context, TemplateError};
use crate::{condition, labels, stylesheet};

/// The hard maximum on firings of any node in a loop, and the value Fabro's
/// "unlimited" lowers to.
pub const MAX_FIRINGS: u32 = 500;

/// The most items a `for_each` fan-out may expand, as Fabro caps it.
pub const MAX_FOR_EACH_ITEMS: u64 = 1_000;

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
}

/// Lower a semantic workflow. `file` is the name spans carry; `files` reads
/// `@file` references and child workflows relative to the repository root.
pub(crate) fn lower(
    mut workflow: Workflow,
    file: &str,
    files: &dyn FileSource,
    inputs: &CompileInputs,
    mut diags: Diagnostics,
) -> Lowered {
    let mut template = Context::new(inputs);
    read_input_defaults(file, files, &mut template, &mut diags);

    let mut b = GraphBuilder::bare();
    let scope = b.add_scope(Scope::new(ScopeId::new(0)));
    let base_dir = match file.rfind('/') {
        Some(index) => file[..index].to_string(),
        None => String::new(),
    };
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
    };

    ctx.graph_attrs(&mut workflow);
    if !ctx.structure(&workflow) {
        return Lowered::rejected(ctx.diags);
    }
    ctx.kinds(&workflow);

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
    let exit = ctx.exit_node(&workflow);
    let goal_check = ctx.goal_check(&workflow, exit);
    for (node, res) in workflow.nodes.iter().zip(&resolved) {
        ctx.routing(node, res, &workflow, goal_check);
    }
    ctx.parallel(&workflow, &resolved);
    let start = ctx.ids[&Ctx::start_id(&workflow)];
    ctx.b.mark_entry(start);
    ctx.back_edges(start);
    ctx.joins_and_budgets(&workflow, &resolved);

    if ctx.diags.has_errors() {
        return Lowered::rejected(ctx.diags);
    }
    let Ctx {
        mut diags,
        b,
        spans,
        template,
        goal,
        ..
    } = ctx;
    let mut graph = b.build();
    graph.completion = Completion::TerminalNode(exit);
    graph.params.insert(
        SmolStr::new("inputs"),
        Value::Object(
            template
                .inputs()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
    );
    graph.params.insert(
        SmolStr::new("vars"),
        Value::Object(
            template
                .vars()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
    );
    graph
        .params
        .insert(SmolStr::new("goal"), Value::String(goal));

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
    Lowered::from_parts(graph, diags)
}

/// `workflow.toml` beside the workflow: `[run.inputs]` holds the defaults an
/// input takes when the host supplies none.
fn read_input_defaults(
    file: &str,
    files: &dyn FileSource,
    template: &mut Context,
    diags: &mut Diagnostics,
) {
    let dir = file.rfind('/').map_or("", |i| &file[..i]);
    let path = if dir.is_empty() {
        "workflow.toml".to_string()
    } else {
        format!("{dir}/workflow.toml")
    };
    let Some(text) = files.read(&path) else {
        return;
    };
    let value: toml::Table = match text.parse() {
        Ok(value) => value,
        Err(error) => {
            diags.warning(
                "fabro.workflow_toml",
                Span::file(&path),
                format!("`{path}` is not valid TOML and its input defaults are ignored: {error}"),
            );
            return;
        }
    };
    let Some(inputs) = value
        .get("run")
        .and_then(|run| run.get("inputs"))
        .and_then(toml::Value::as_table)
    else {
        return;
    };
    for (name, value) in inputs {
        let json = serde_json::to_value(value).unwrap_or(Value::Null);
        template.default_input(name, json);
    }
}

fn placeholder(id: ExprId) -> Value {
    json!({ EXPR_PLACEHOLDER_KEY: id.raw() })
}

fn duration_ms(duration: Duration) -> Value {
    Value::from(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

impl Ctx<'_> {
    fn unknown_attrs(&mut self, attrs: &Attrs, known: &[&str], what: &str) {
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
            if key == "import" {
                self.diags.unsupported(
                    "import",
                    attr.span.clone(),
                    "workflow imports are not supported in phase one",
                    "inline the imported nodes; see .ai/plans/fabro-frontend-phase-one.md §4.2",
                );
                continue;
            }
            if key == "auto_status" {
                self.diags.unsupported(
                    "auto_status",
                    attr.span.clone(),
                    "`auto_status` is a deprecated spelling of `on_failure=\"succeed\"`, which \
                     records a clean success for a step that failed",
                    "use `on_failure=\"partially_succeed\"`, which keeps the failure on the record",
                );
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
            if known.contains(&key) || attrs::LAYOUT.contains(&key) {
                continue;
            }
            if let Some((_, why)) = attrs::NODE_IGNORED.iter().find(|(k, _)| *k == key) {
                self.diags.warning(
                    &format!("ignored.{key}"),
                    attr.span.clone(),
                    format!("`{key}` on {what} is ignored: {why}"),
                );
                continue;
            }
            self.diags.warning(
                "fabro.unknown_attribute",
                attr.span.clone(),
                format!("`{key}` on {what} is not a Fabro attribute and is ignored"),
            );
        }
    }

    fn template_error(&mut self, error: &TemplateError, span: &Span, what: &str) {
        match error {
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
    /// `{% include %}` resolves beside the file it appears in.
    fn rendered(&mut self, text: &str, span: &Span, what: &str) -> Option<String> {
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
                self.template_error(&error, span, what);
                None
            }
        }
    }

    // ── Graph level ────────────────────────────────────────────────────────

    fn graph_attrs(&mut self, workflow: &mut Workflow) {
        let known: Vec<&str> = attrs::GRAPH
            .iter()
            .copied()
            .chain(attrs::GRAPH_IGNORED.iter().map(|(k, _)| *k))
            .collect();
        let attrs = workflow.attrs.clone();
        self.unknown_attrs(&attrs, &known, "the graph");
        for (key, why) in attrs::GRAPH_IGNORED {
            if let Some(attr) = attrs.get(key) {
                self.diags.warning(
                    &format!("ignored.{key}"),
                    attr.span.clone(),
                    format!("graph attribute `{key}` is ignored: {why}"),
                );
            }
        }
        let span = workflow.span.clone();
        let goal_span = attrs.span_of("goal", &span);
        let goal = attrs.text("goal").unwrap_or_default();
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
            if let Some(rendered) = self.rendered(&sheet, &sheet_span, "the `model_stylesheet`") {
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
        if let Some(policy) = attrs.text("on_failure")
            && Policy::parse(&policy).is_none()
        {
            self.bad_policy("on_failure", &policy, &attrs.span_of("on_failure", &span));
        }
        if let Some(policy) = attrs.text("on_retries_exhausted")
            && Policy::parse(&policy).is_none()
        {
            self.bad_policy(
                "on_retries_exhausted",
                &policy,
                &attrs.span_of("on_retries_exhausted", &span),
            );
        }
    }

    fn bad_policy(&mut self, key: &str, value: &str, span: &Span) {
        if value == "succeed" {
            self.diags.unsupported(
                &format!("{key}.succeed"),
                span.clone(),
                format!(
                    "`{key}=\"succeed\"` would record a clean success for a step that failed, \
                     which the event log forbids"
                ),
                "use `partially_succeed`: the node routes as a success and keeps the failure on \
                 its record",
            );
        } else {
            self.diags.error(
                "fabro.bad_on_failure",
                span.clone(),
                format!("`{key}` must be `route`, `exit` or `partially_succeed`, not `{value}`"),
            );
        }
    }

    // ── Structure ──────────────────────────────────────────────────────────

    fn start_id(workflow: &Workflow) -> String {
        workflow
            .nodes
            .iter()
            .find(|n| shape_of(n) == "Mdiamond")
            .or_else(|| workflow.node("start"))
            .or_else(|| workflow.node("Start"))
            .map(|n| n.id.clone())
            .unwrap_or_default()
    }

    fn exit_id(workflow: &Workflow) -> Option<String> {
        workflow
            .nodes
            .iter()
            .find(|n| shape_of(n) == "Msquare")
            .or_else(|| {
                ["exit", "Exit", "end", "End"]
                    .iter()
                    .find_map(|id| workflow.node(id))
            })
            .map(|n| n.id.clone())
    }

    fn exit_node(&self, workflow: &Workflow) -> NodeId {
        let id = Self::exit_id(workflow).expect("structure() checked the exit");
        self.ids[&id]
    }

    /// The checks a graph must pass before lowering makes sense. False means
    /// stop: the diagnostics already say why.
    fn structure(&mut self, workflow: &Workflow) -> bool {
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
        let start = Self::start_id(workflow);
        if start.is_empty() {
            self.diags.error(
                "fabro.no_start",
                workflow.span.clone(),
                "the workflow has no start node (`shape=Mdiamond`, or an id of `start`)",
            );
            return false;
        }
        let Some(exit) = Self::exit_id(workflow) else {
            self.diags.error(
                "fabro.no_exit",
                workflow.span.clone(),
                "the workflow has no exit node (`shape=Msquare`, or an id of `exit`)",
            );
            return false;
        };
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
        ok
    }

    fn kinds(&mut self, workflow: &Workflow) {
        for node in &workflow.nodes {
            let kind = self.kind_of(node);
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
        let known: Vec<&str> = attrs::NODE
            .iter()
            .copied()
            .chain(attrs::NODE_IGNORED.iter().map(|(k, _)| *k))
            .collect();
        self.unknown_attrs(&node.attrs, &known, &format!("node `{}`", node.id));
        let kind = self.kinds[&node.id];
        let id = self.ids[&node.id];
        let shape = shape_of(node);
        let label = node.attrs.text("label").unwrap_or_else(|| node.id.clone());
        for key in ["on_failure", "on_retries_exhausted"] {
            if let Some(value) = node.attrs.text(key)
                && Policy::parse(&value).is_none()
            {
                self.bad_policy(key, &value, &node.attrs.span_of(key, &node.span));
            }
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
            Kind::Start | Kind::Exit | Kind::Conditional => (None, STRUCTURAL_TIMEOUT),
            Kind::FanIn => {
                if node.attrs.contains("prompt") {
                    self.diags.unsupported(
                        "fan_in.prompt",
                        node.attrs.span_of("prompt", &node.span),
                        "a fan-in node with a `prompt` runs an agent over the branch results",
                        "join with a plain `tripleoctagon`, then read the results in the next node",
                    );
                }
                (None, STRUCTURAL_TIMEOUT)
            }
            Kind::Parallel => (None, explicit.unwrap_or(STRUCTURAL_TIMEOUT)),
            Kind::Agent | Kind::Prompt => {
                let config = self.agent_config(node, kind, workflow, policy, explicit);
                (
                    Some(StepRef::new(AGENT_KIND, config)),
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
                let config = self.workflow_config(node);
                (
                    Some(StepRef::new(WORKFLOW_KIND, config)),
                    explicit.unwrap_or(AGENT_TIMEOUT),
                )
            }
        };
        if let Some(step) = step {
            self.b.node_mut(id).step = step;
        }
        self.b.node_mut(id).budget = Budget::new(1, timeout);
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
        config
    }

    fn fidelity(&mut self, node: &NodeDecl, workflow: &Workflow) -> Option<String> {
        let (value, span) = match node.attrs.text("fidelity") {
            Some(value) => (value, node.attrs.span_of("fidelity", &node.span)),
            None => (
                workflow.attrs.text("default_fidelity")?,
                workflow.attrs.span_of("default_fidelity", &workflow.span),
            ),
        };
        if !attrs::FIDELITIES.contains(&value.as_str()) {
            self.diags.error(
                "fabro.bad_fidelity",
                span,
                format!(
                    "`{value}` is not a fidelity mode ({})",
                    attrs::FIDELITIES.join(", ")
                ),
            );
            return None;
        }
        Some(value)
    }

    fn agent_config(
        &mut self,
        node: &NodeDecl,
        kind: Kind,
        workflow: &Workflow,
        policy: FailurePolicy,
        timeout: Option<Duration>,
    ) -> Value {
        let mut config = self.base_config(node, policy, timeout);
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
        for key in [
            "model",
            "provider",
            "reasoning_effort",
            "speed",
            "backend",
            "thread_id",
            "max_tokens",
        ] {
            if let Some(value) = node.attrs.text(key) {
                config.insert(key.into(), Value::String(value));
            }
        }
        if !config.contains_key("model")
            && let Some(model) = workflow.attrs.text("default_model")
        {
            config.insert("model".into(), Value::String(model));
        }
        if !config.contains_key("provider")
            && let Some(provider) = workflow.attrs.text("default_provider")
        {
            config.insert("provider".into(), Value::String(provider));
        }
        if let Some(fidelity) = self.fidelity(node, workflow) {
            config.insert("fidelity".into(), Value::String(fidelity));
        }
        if let Some(memory) = node.attrs.bool("project_memory", &mut self.diags) {
            config.insert("project_memory".into(), Value::Bool(memory));
        }
        self.output_schema(node, &mut config);
        if let Some(retries) = node.attrs.int("output_retries", &mut self.diags) {
            config.insert("output_retries".into(), Value::from(retries.max(0)));
        }
        self.acp(node, workflow, &mut config);
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
        let mut config = self.base_config(node, policy, timeout);
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
                        self.template_error(&error, &span, &format!("node `{}` `script`", node.id));
                    }
                }
            }
            _ => self.diags.error(
                "fabro.command_requires_script",
                node.span.clone(),
                format!("command node `{}` needs a `script`", node.id),
            ),
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
        if key == "parallel.results" {
            if let Some(fan_in) = self.nearest_fan_in(&node.id, workflow) {
                return Some(self.b.exprs().path("nodes", &[&fan_in, "output"]));
            }
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

    fn nearest_fan_in(&self, id: &str, workflow: &Workflow) -> Option<String> {
        let mut seen: HashSet<&str> = HashSet::from([id]);
        let mut queue: VecDeque<&str> = VecDeque::from([id]);
        while let Some(current) = queue.pop_front() {
            for edge in workflow.incoming(current) {
                if self.kinds.get(&edge.from) == Some(&Kind::FanIn) {
                    return Some(edge.from.clone());
                }
                if seen.insert(&edge.from) {
                    queue.push_back(&edge.from);
                }
            }
        }
        None
    }

    fn human_config(
        &mut self,
        node: &NodeDecl,
        workflow: &Workflow,
        policy: FailurePolicy,
        timeout: Option<Duration>,
    ) -> Value {
        let mut config = self.base_config(node, policy, timeout);
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
        if let Some(review) = node.attrs.bool("review_target", &mut self.diags) {
            config.insert("review_target".into(), Value::Bool(review));
        }
        if let Some(sensitive) = node.attrs.bool("sensitive", &mut self.diags) {
            config.insert("sensitive".into(), Value::Bool(sensitive));
        }
        config.insert("choices".into(), Value::Array(choices));
        if let Some(target) = freeform_target {
            config.insert("freeform_target".into(), Value::String(target));
        }
        Value::Object(config)
    }

    fn workflow_config(&mut self, node: &NodeDecl) -> Value {
        let mut config = Map::new();
        config.insert(
            "label".into(),
            Value::String(node.attrs.text("label").unwrap_or_else(|| node.id.clone())),
        );
        config.insert("node".into(), Value::String(node.id.clone()));
        let kv = self.b.exprs().var("kv");
        config.insert("kv".into(), placeholder(kv));
        if let Some(cycles) = node.attrs.int("manager.max_cycles", &mut self.diags) {
            config.insert("max_cycles".into(), Value::from(cycles.max(0)));
        }
        if let Some(interval) = node
            .attrs
            .duration("manager.poll_interval", &mut self.diags)
        {
            config.insert("poll_interval_ms".into(), duration_ms(interval));
        }
        if let Some(stop) = node.attrs.text("manager.stop_condition") {
            config.insert("stop_condition".into(), Value::String(stop));
        }
        let source = node.attrs.text("stack.child_workflow");
        let inline = node.attrs.text("stack.child_dot_source");
        match (source, inline) {
            (Some(path), _) => {
                config.insert("child_workflow".into(), Value::String(path));
            }
            (None, Some(source)) => {
                config.insert("child_dot_source".into(), Value::String(source));
            }
            (None, None) => self.diags.error(
                "fabro.manager_loop_without_child",
                node.span.clone(),
                format!(
                    "manager loop `{}` needs `stack.child_workflow` or `stack.child_dot_source`",
                    node.id
                ),
            ),
        }
        Value::Object(config)
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
        let check = self
            .b
            .add_node("goal_check", self.scope, StepRef::new("noop", Value::Null));
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
        let exit = self.exit_node(workflow);
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
        let policy = res.policy;
        let mut lowered = Vec::with_capacity(edges.len());
        for (index, edge) in edges.iter().enumerate() {
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
                &format!("edge `{} -> {}`", edge.from, edge.to),
            );
            let cond = self.edge_condition(edge, res, policy);
            let weight = match edge.attrs.int("weight", &mut self.diags) {
                Some(w) if w < 0 => {
                    self.diags.error(
                        "fabro.bad_weight",
                        edge.attrs.span_of("weight", &edge.span),
                        "an edge `weight` cannot be negative",
                    );
                    0
                }
                Some(w) => u32::try_from(w).unwrap_or(u32::MAX),
                None => 0,
            };
            let weight = if random { weight.max(1) } else { weight };
            let label = edge.attrs.text("label").filter(|l| !l.is_empty());
            let restart = edge
                .attrs
                .bool("loop_restart", &mut self.diags)
                .unwrap_or(false);
            let map = self.branch_payload(edge, index, res.kind);
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
        self.b.node_mut(res.id).routing = Routing::groups(vec![group]);
    }

    fn edge_condition(
        &mut self,
        edge: &EdgeDecl,
        res: &Resolved,
        policy: FailurePolicy,
    ) -> Option<ExprId> {
        let text = edge.attrs.text("condition")?;
        if text.trim().is_empty() {
            return None;
        }
        let span = edge.attrs.span_of("condition", &edge.span);
        if policy.on_failure == Policy::PartiallySucceed
            && condition::parse(&text).is_ok_and(|c| mentions_failed(&c))
        {
            self.diags.warning(
                "fabro.unreachable_failure_edge",
                span.clone(),
                format!(
                    "`{}` has `on_failure=\"partially_succeed\"`, so a non-retryable failure is \
                     classified as a partial success before routing sees it, and this \
                     `outcome=failed` edge can never match. Fabro would take it; under Petri the \
                     outcome is classified once, at the step boundary. Use two nodes for both \
                     behaviors",
                    res.id
                ),
            );
        }
        condition::lower(&text, self.b.exprs(), &span, &mut self.diags)
    }

    /// The payload a branch sends to a fan-in: `{ index, value: { id, status,
    /// output } }`, so the fan-in can put branches back in order.
    fn branch_payload(&mut self, edge: &EdgeDecl, index: usize, kind: Kind) -> Option<ExprId> {
        if self.kinds.get(&edge.to) != Some(&Kind::FanIn) || kind == Kind::FanIn {
            return None;
        }
        let exprs = self.b.exprs();
        let index_expr = {
            // A clone of a `for_each` template carries `index`; a static branch
            // is its position among the fan-out's arms.
            let bound = exprs.var("index");
            let position = exprs.lit(u64::try_from(index).unwrap_or(u64::MAX));
            exprs.call("default", vec![bound, position])
        };
        let id = exprs.lit(edge.from.as_str());
        let status = exprs.var("status");
        let output = exprs.var("output");
        let value = exprs.object(vec![("id", id), ("status", status), ("output", output)]);
        Some(exprs.object(vec![("index", index_expr), ("value", value)]))
    }

    // ── Parallel ───────────────────────────────────────────────────────────

    fn parallel(&mut self, workflow: &Workflow, resolved: &[Resolved]) {
        for (node, res) in workflow.nodes.iter().zip(resolved) {
            match res.kind {
                Kind::Parallel => self.fan_out(node, res, workflow),
                Kind::FanIn => {
                    let ordered = {
                        let exprs = self.b.exprs();
                        let inputs = exprs.var("inputs");
                        let index = exprs.lit("index");
                        let sorted = exprs.call("sort_by_key", vec![inputs, index]);
                        let value = exprs.lit("value");
                        exprs.call("pluck", vec![sorted, value])
                    };
                    self.b.node_mut(res.id).step = StepRef::new("noop", placeholder(ordered));
                }
                _ => {}
            }
        }
    }

    fn fan_out(&mut self, node: &NodeDecl, res: &Resolved, workflow: &Workflow) {
        let edges = workflow.outgoing(&node.id);
        for edge in &edges {
            if edge
                .attrs
                .text("condition")
                .is_some_and(|c| !c.trim().is_empty())
            {
                self.diags.error(
                    "fabro.parallel.conditional_branch",
                    edge.span.clone(),
                    "a parallel node's branches are unconditional",
                );
            }
        }
        if edges.is_empty() {
            self.diags.error(
                "fabro.parallel.no_branches",
                node.span.clone(),
                format!("parallel node `{}` has no branches", node.id),
            );
            return;
        }
        let Some(source) = node.attrs.text("for_each") else {
            let targets: Vec<NodeId> = edges
                .iter()
                .filter_map(|e| self.ids.get(&e.to).copied())
                .collect();
            self.b.fan_out(res.id, &targets);
            return;
        };
        let span = node.attrs.span_of("for_each", &node.span);
        let key = source
            .strip_prefix("context.")
            .unwrap_or(&source)
            .trim()
            .to_string();
        if key.is_empty() {
            self.diags.error(
                "fabro.for_each.source",
                span,
                format!("`for_each` on `{}` must name a context key", node.id),
            );
            return;
        }
        if edges.len() != 1 {
            self.diags.error(
                "fabro.for_each.template_edges",
                node.span.clone(),
                format!(
                    "`for_each` node `{}` needs exactly one template edge",
                    node.id
                ),
            );
            return;
        }
        let target_decl = &edges[0];
        let Some(target) = self.ids.get(&target_decl.to).copied() else {
            return;
        };
        let target_kind = self.kinds[&target_decl.to];
        if !target_kind.is_llm() {
            self.diags.error(
                "fabro.for_each.target",
                target_decl.to_span.clone(),
                format!(
                    "the `for_each` template `{}` must be an agent or prompt node",
                    target_decl.to
                ),
            );
            return;
        }
        if workflow
            .node(&target_decl.to)
            .is_some_and(|n| n.attrs.contains("for_each"))
        {
            self.diags.error(
                "fabro.for_each.nested",
                target_decl.to_span.clone(),
                "nested `for_each` is not supported",
            );
            return;
        }
        let max_parallel = node
            .attrs
            .int("max_parallel", &mut self.diags)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0);

        // The parallel node evaluates the item array and hands it on; the
        // template node expands over its input.
        let items = {
            let exprs = self.b.exprs();
            let kv = exprs.var("kv");
            let name = exprs.lit(key.as_str());
            exprs.call("get", vec![kv, name])
        };
        self.b.node_mut(res.id).step = StepRef::new("noop", placeholder(items));
        let cap = {
            let exprs = self.b.exprs();
            let kv = exprs.var("kv");
            let name = exprs.lit(key.as_str());
            let items = exprs.call("get", vec![kv, name]);
            let len = exprs.call("len", vec![items]);
            let limit = exprs.lit(MAX_FOR_EACH_ITEMS);
            exprs.binary(ir::BinOp::Le, len, limit)
        };
        self.b.set_precondition(res.id, cap);
        self.b.link(res.id, target);
        let input = self.b.exprs().var("input");
        ir::parallel_for_each(
            &mut self.b,
            target,
            input,
            ExpandTarget::Node,
            max_parallel,
            false,
        );
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
        let count = self.b.graph().nodes.len();
        let mut color = vec![Color::White; count];
        let mut back: HashSet<EdgeId> = HashSet::new();
        // (node, index of the next edge to visit)
        let mut stack: Vec<(NodeId, usize)> = vec![(start, 0)];
        color[start.index()] = Color::Gray;
        let successors = |graph: &ir::Graph, node: NodeId| -> Vec<(EdgeId, NodeId)> {
            graph
                .node(node)
                .map(|n| n.routing.edges().map(|e| (e.id, e.to)).collect())
                .unwrap_or_default()
        };
        while let Some((node, next)) = stack.last().copied() {
            let edges = successors(self.b.graph(), node);
            if next >= edges.len() {
                color[node.index()] = Color::Black;
                stack.pop();
                continue;
            }
            stack.last_mut().expect("non-empty").1 += 1;
            let (edge, to) = edges[next];
            match color[to.index()] {
                Color::Gray => {
                    back.insert(edge);
                }
                Color::White => {
                    color[to.index()] = Color::Gray;
                    stack.push((to, 0));
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

    fn joins_and_budgets(&mut self, workflow: &Workflow, resolved: &[Resolved]) {
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
            let timeout = self
                .b
                .graph()
                .node(res.id)
                .map_or(STRUCTURAL_TIMEOUT, |n| n.budget.timeout);
            self.b.set_budget(res.id, Budget::new(max_firings, timeout));
        }
        // The synthetic goal check, when it exists, loops too.
        let goal_check = self
            .b
            .graph()
            .nodes
            .iter()
            .find(|n| n.name == "goal_check")
            .map(|n| n.id);
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

fn mentions_failed(condition: &condition::Condition) -> bool {
    match condition {
        condition::Condition::Clause(c) => {
            c.key == "outcome" && c.value == "failed" && c.op == condition::Op::Eq
        }
        condition::Condition::Not(inner) => mentions_failed(inner),
        condition::Condition::And(items) | condition::Condition::Or(items) => {
            items.iter().any(mentions_failed)
        }
    }
}
