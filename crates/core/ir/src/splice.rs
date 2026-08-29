//! The outcome-splice vocabulary: what a step's final outcome may ask the engine
//! to graft into the live graph, and the capability that gates it.
//!
//! A [`SpliceRequest`] is fragment-local, serialized inside `StepFinished`, and
//! untrusted: every id in its [`GraphFragment`] lives in the [`Local`] space and
//! means nothing against the live graph until the engine's preparation remaps it.
//! Policy is a closed capability, not a Boolean: [`SplicePolicy`] is totally
//! ordered, `authorize` rejects an operation above the node's policy, and the
//! delegation check rejects a fragment node whose declared policy exceeds its
//! uploader's. Both reject loudly as `invalid_splice` — never a silent clamp.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::graph::{Completion, Graph, Node, Scope};
use crate::ids::{Local, NodeId};
use crate::step::StepKinds;
use crate::validate::{self, ValidationError};
use crate::{Expansion, ExprTable};

// ── Policy ────────────────────────────────────────────────────────────────

/// What retraction a `Replace` may perform, and the upper half of the policy
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReplaceScope {
    /// Retract only batches whose recorded owner is the retracting uploader's
    /// `NodeId`. Ownership is core-derived, stamped at apply time; a request
    /// carries no identity fields.
    OwnBatches,
    /// Run-scoped destructive authority: every retractable admission qualifies.
    AllPending,
}

/// A node's splice capability. **Totally ordered**:
/// `Deny < Append < Replace(OwnBatches) < Replace(AllPending)` — the derived
/// order *is* the authority order, so `authorizes` and `may_delegate` are
/// comparisons, not matches that could drift.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum SplicePolicy {
    /// The default: every request from this node is `invalid_splice`.
    #[default]
    Deny,
    Append,
    Replace {
        scope: ReplaceScope,
    },
}

impl SplicePolicy {
    /// Whether this policy authorizes `mode`. An operation above the policy is
    /// rejected loudly by the engine as `invalid_splice`; nothing is clamped.
    pub fn authorizes(self, mode: &SpliceMode) -> bool {
        self >= mode.required_policy()
    }

    /// The delegation check: a fragment node may declare at most its uploader's
    /// policy. This is a legality check, never a mutation — excess authority
    /// rejects, and the declared policy is never silently changed.
    pub fn may_delegate(self, granted: SplicePolicy) -> bool {
        granted <= self
    }
}

/// What a request asks for. The request carries its retraction scope; policy
/// only authorizes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpliceMode {
    /// Add the fragment; retract nothing.
    Append,
    /// Retract the retractable admissions in `scope`, then add the fragment
    /// (which may be empty: a pure retraction).
    Replace { scope: ReplaceScope },
}

impl SpliceMode {
    /// The least policy that authorizes this operation.
    pub fn required_policy(&self) -> SplicePolicy {
        match self {
            SpliceMode::Append => SplicePolicy::Append,
            SpliceMode::Replace { scope } => SplicePolicy::Replace { scope: *scope },
        }
    }
}

// ── The fragment ──────────────────────────────────────────────────────────

/// An executable-plan fragment, in its own [`Local`] id space.
///
/// Reuses the live graph's node/edge/scope/expression types over `Local` ids
/// rather than a parallel mirror family; the type system refuses a mixed-space
/// id, and only the engine's preparation remapper converts. A fragment owns no
/// run `params`, no root entries and no `completion` — those belong to the run.
/// V1 fragments declare their own resource scopes only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphFragment {
    pub nodes: Vec<Node<Local>>,
    pub scopes: Vec<Scope<Local>>,
    pub exprs: ExprTable<Local>,
    /// Where the fragment starts: these nodes implicitly attach to the uploader
    /// through new select groups on it.
    pub entries: Vec<NodeId<Local>>,
    /// Where the fragment ends: exits gain edges to each existing dependent of
    /// the uploader, so a dependency on the uploader becomes a dependency on the
    /// batch too.
    pub exits: Vec<NodeId<Local>>,
}

impl GraphFragment {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A reference to a node that already exists outside the fragment: an instance
/// name (the `#` scheme included), never a live `NodeId` — a request is
/// untrusted and fragment-local, so it cannot speak live ids. Resolved through
/// the preparation name index, which covers the live graph and the nodes added
/// by earlier requests in the same outcome.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExistingNodeRef(SmolStr);

impl ExistingNodeRef {
    pub fn new(name: &str) -> Self {
        Self(SmolStr::new(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExistingNodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a fragment ties into work outside itself. Concrete stage-1 IR on the
/// request, not on the fragment: entries implicitly attach to the uploader and
/// need no declaration here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Attachment {
    /// `node` (fragment-local) additionally depends on an existing node. By
    /// state at splice time: a not-final reference extends that node's routing
    /// with a real edge; a final reference becomes a guard over
    /// `nodes.<name>.status`. A reference must resolve to exactly one
    /// admission — any loop node rejects in v1.
    DependsOn {
        node: NodeId<Local>,
        on: ExistingNodeRef,
    },
}

/// One ordered entry in `Outcome::splices`. All requests in one final outcome
/// prepare successfully or none apply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpliceRequest {
    pub mode: SpliceMode,
    pub fragment: GraphFragment,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

impl SpliceRequest {
    pub fn append(fragment: GraphFragment) -> Self {
        Self {
            mode: SpliceMode::Append,
            fragment,
            attachments: Vec::new(),
        }
    }

    pub fn replace(scope: ReplaceScope, fragment: GraphFragment) -> Self {
        Self {
            mode: SpliceMode::Replace { scope },
            fragment,
            attachments: Vec::new(),
        }
    }

    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }
}

// ── Fragment validation ───────────────────────────────────────────────────

/// Something wrong inside one fragment. It cannot know a request index — the
/// engine's preparation wraps it with one; only the engine boundary maps either
/// to `invalid_splice`.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{location}: {kind}")]
pub struct FragmentValidationError {
    /// Where in the fragment: `node N`, `edge N`, `entry N`, `attachment N`, or
    /// `fragment` for whole-fragment problems.
    pub location: SmolStr,
    pub kind: FragmentErrorKind,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum FragmentErrorKind {
    /// A §8 structural invariant, from the one shared invariant engine run over
    /// the fragment-local view.
    #[error(transparent)]
    Structure(ValidationError<Local>),
    #[error("an empty fragment is valid only under Replace; an empty Append has nothing to add")]
    EmptyAppend,
    #[error("exit list refers to unknown node {0:?}")]
    UnknownExit(NodeId<Local>),
    #[error("exit node {0:?} is listed twice")]
    DuplicateExit(NodeId<Local>),
    #[error("node {0:?} carries an `expand`; a fragment is executable IR, never HIR")]
    ExpansionInFragment(NodeId<Local>),
    #[error("attachment names unknown fragment node {0:?}")]
    AttachmentUnknownNode(NodeId<Local>),
}

/// Validate one fragment: the §8 invariant engine over a fragment-local view —
/// the same checks a live graph gets, no second validator that can drift — plus
/// the fragment-only rules (exits resolve, no HIR `expand`).
pub fn validate_fragment(fragment: &GraphFragment) -> Result<(), Vec<FragmentValidationError>> {
    validate_fragment_with(fragment, None)
}

/// [`validate_fragment`], resolving every step kind against `registry` — the
/// host half of the two-stage contract, runnable before any run exists.
pub fn validate_fragment_with(
    fragment: &GraphFragment,
    registry: Option<&dyn StepKinds>,
) -> Result<(), Vec<FragmentValidationError>> {
    let mut errors = Vec::new();

    // The shared invariant engine, over the fragment as a graph of its own.
    // An empty fragment is structurally trivial; running the engine on it would
    // only report `NoEntry`, and emptiness is a mode question, not a structure
    // question — `validate_request` owns it.
    if !fragment.nodes.is_empty() {
        let view = Graph::<Local> {
            nodes: fragment.nodes.clone(),
            scopes: fragment.scopes.clone(),
            exprs: fragment.exprs.clone(),
            entry: fragment.entries.clone(),
            params: Default::default(),
            completion: Completion::AnyFailure,
        };
        for error in validate::collect(&view, registry) {
            errors.push(FragmentValidationError {
                location: structure_location(&error),
                kind: FragmentErrorKind::Structure(error),
            });
        }
    }

    let mut seen_exits = std::collections::BTreeSet::new();
    for exit in &fragment.exits {
        if exit.index() >= fragment.nodes.len() {
            errors.push(FragmentValidationError {
                location: SmolStr::new(format!("exit {exit}")),
                kind: FragmentErrorKind::UnknownExit(*exit),
            });
            continue;
        }
        if !seen_exits.insert(*exit) {
            errors.push(FragmentValidationError {
                location: SmolStr::new(format!("exit {exit}")),
                kind: FragmentErrorKind::DuplicateExit(*exit),
            });
        }
    }

    for node in &fragment.nodes {
        if let Some(Expansion::ForEach { .. }) = node.expand {
            errors.push(FragmentValidationError {
                location: SmolStr::new(format!("node {}", node.id)),
                kind: FragmentErrorKind::ExpansionInFragment(node.id),
            });
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// Validate a whole request: the fragment, the mode/emptiness rule (an empty
/// fragment is valid only under `Replace`), and that every attachment names a
/// fragment node that exists. The existing-node half of an attachment needs
/// live state and stays with the engine.
pub fn validate_request(request: &SpliceRequest) -> Result<(), Vec<FragmentValidationError>> {
    validate_request_with(request, None)
}

/// [`validate_request`] with a step registry, mirroring [`validate_fragment_with`].
pub fn validate_request_with(
    request: &SpliceRequest,
    registry: Option<&dyn StepKinds>,
) -> Result<(), Vec<FragmentValidationError>> {
    let mut errors = match validate_fragment_with(&request.fragment, registry) {
        Ok(()) => Vec::new(),
        Err(errors) => errors,
    };

    if request.fragment.nodes.is_empty() && request.mode == SpliceMode::Append {
        errors.push(FragmentValidationError {
            location: SmolStr::new("fragment"),
            kind: FragmentErrorKind::EmptyAppend,
        });
    }

    for (index, attachment) in request.attachments.iter().enumerate() {
        let Attachment::DependsOn { node, .. } = attachment;
        if node.index() >= request.fragment.nodes.len() {
            errors.push(FragmentValidationError {
                location: SmolStr::new(format!("attachment {index}")),
                kind: FragmentErrorKind::AttachmentUnknownNode(*node),
            });
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// The fragment location a structural error anchors to.
fn structure_location(error: &ValidationError<Local>) -> SmolStr {
    use ValidationError as E;
    let text = match error {
        E::NodeIdMismatch { index, .. } => format!("node {index}"),
        E::ScopeIdMismatch { index, .. } => format!("scope {index}"),
        E::UnknownScope { node, .. }
        | E::UnknownStepKind { node, .. }
        | E::BadStepConfig { node, .. }
        | E::AlwaysNotLast { node, .. }
        | E::EmptyGroup { node, .. }
        | E::ExitUnreachable { node, .. }
        | E::ExitNotPostdominator { node, .. }
        | E::BoundaryCrossing { node, .. }
        | E::HirConfigInPlan { node, .. } => format!("node {node}"),
        E::UnknownTarget { edge, .. } => format!("edge {edge}"),
        E::NoEntry => "fragment".to_string(),
        E::UnknownEntry(node) | E::DuplicateEntry(node) | E::EntryHasIncoming(node) => {
            format!("entry {node}")
        }
        E::CompletionUnknownNode(_) => "fragment".to_string(),
        E::CycleWithoutBackEdge(nodes) => match nodes.first() {
            Some(node) => format!("node {node}"),
            None => "fragment".to_string(),
        },
        E::ZeroBudget(node)
        | E::UnboundedLoopBudget(node)
        | E::LoopHeadMustJoinAny(node)
        | E::HirFieldInPlan(node) => format!("node {node}"),
        E::DuplicateEdgeId(edge) | E::ReservedEdgeId(edge) => format!("edge {edge}"),
        E::UnknownExpr { site, .. } => site.to_string(),
    };
    SmolStr::new(text)
}
