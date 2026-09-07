//! Which attributes mean something, which are pure Graphviz layout, and
//! which are refused. Anything not listed here is diagnosed: a value that
//! silently does nothing is the failure this table exists to prevent.

/// Graphviz attributes that only affect a rendering. Dropped without a word:
/// they carry no workflow meaning and every real `.fabro` file has some.
pub(super) const LAYOUT: &[&str] = &[
    "arrowhead",
    "arrowsize",
    "arrowtail",
    "bgcolor",
    "center",
    "clusterrank",
    "color",
    "comment",
    "compound",
    "concentrate",
    "constraint",
    "decorate",
    "dir",
    "distortion",
    "dpi",
    "fillcolor",
    "fixedsize",
    "fontcolor",
    "fontname",
    "fontsize",
    "group",
    "headport",
    "height",
    "href",
    "id",
    "image",
    "imagescale",
    "labelangle",
    "labeldistance",
    "labelfloat",
    "labeljust",
    "labelloc",
    "layout",
    "lhead",
    "ltail",
    "margin",
    "mclimit",
    "minlen",
    "newrank",
    "nodesep",
    "nojustify",
    "nslimit",
    "ordering",
    "orientation",
    "outputorder",
    "overlap",
    "pad",
    "page",
    "pencolor",
    "penwidth",
    "peripheries",
    "pos",
    "rank",
    "rankdir",
    "ranksep",
    "ratio",
    "regular",
    "remincross",
    "rotate",
    "samehead",
    "sametail",
    "searchsize",
    "shapefile",
    "sides",
    "size",
    "skew",
    "splines",
    "style",
    "tailport",
    "target",
    "tooltip",
    "URL",
    "width",
    "xlabel",
];

/// Graph-level attributes with workflow meaning.
pub(super) const GRAPH: &[&str] = &[
    "goal",
    "default_thread",
    "label",
    "backend",
    "model_stylesheet",
    "default_max_retries",
    "retry_target",
    "fallback_retry_target",
    "on_failure",
    "on_retries_exhausted",
    "default_fidelity",
    "default_model",
    "default_provider",
    "max_node_visits",
    "selection",
    "acp.command",
    "acp.config",
    "stall_timeout",
    "loop_restart_signature_limit",
];

/// Graph-level attributes Petri accepts but does not act on, each named in
/// an `ignored.*` warning. Empty: every graph attribute Fabro defines is
/// acted on.
pub(super) const GRAPH_IGNORED: &[(&str, &str)] = &[];

/// Node attributes with workflow meaning.
pub(super) const NODE: &[&str] = &[
    "label",
    "shape",
    "type",
    "backend",
    "prompt",
    "script",
    "language",
    "for_each",
    "max_parallel",
    "stdin_source",
    "output_schema",
    "output_retries",
    "max_retries",
    "retry_policy",
    "max_visits",
    "goal_gate",
    "retry_target",
    "fallback_retry_target",
    "on_failure",
    "on_retries_exhausted",
    "allow_partial",
    "fidelity",
    "thread_id",
    "project_memory",
    "speed",
    "max_tokens",
    "timeout",
    "model",
    "provider",
    "reasoning_effort",
    "acp.command",
    "acp.config",
    "selection",
    "class",
    "question_type",
    "sensitive",
    "review_target",
    "human.default_choice",
    "duration",
    "stack.child_workflow",
    "stack.child_dot_source",
    "manager.max_cycles",
    "manager.poll_interval",
    "manager.stop_condition",
];

/// Node attributes Petri accepts but does not act on, each named in an
/// `ignored.*` warning. Empty: every node attribute Fabro defines is acted
/// on; `tool_hooks.*` is not one and is diagnosed as unknown.
pub(super) const NODE_IGNORED: &[(&str, &str)] = &[];

/// Edge attributes with workflow meaning.
pub(super) const EDGE: &[&str] = &[
    "label",
    "condition",
    "weight",
    "fidelity",
    "thread_id",
    "loop_restart",
    "freeform",
];

/// The older Attractor dialect, refused as `unsupported.attractor`.
pub(super) const ATTRACTOR: &[&str] = &["llm_prompt", "is_codergen", "node_type"];

/// The Fabro fidelity modes.
pub(super) const FIDELITIES: &[&str] = &[
    "full",
    "truncate",
    "compact",
    "summary:low",
    "summary:medium",
    "summary:high",
];

/// The Fabro question types a human gate may declare.
pub(super) const QUESTION_TYPES: &[&str] = &[
    "yes_no",
    "confirmation",
    "multiple_choice",
    "multi_select",
    "freeform",
];
