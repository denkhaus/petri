//! Reusable-workflow calls, resolved before lowering.
//!
//! The resolver walks the call graph from the root file — depth-first, so a
//! cycle or a too-deep nest is caught with the chain in hand — and fetches and
//! parses every called workflow exactly once: a local `./.github/workflows/…`
//! through the repository's [`FileSource`], a pinned `owner/repo/path@ref`
//! through the [`ActionSource`] (which also serves a remote callee's own `./`
//! calls, against the same pinned repository). Lowering then never fetches
//! mid-pass: it looks callees up here, and the same file lowers to the same
//! graph.
//!
//! The resolver owns each callee's parsed [`Document`]; the borrowed models the
//! lowering works over are re-read from these documents ([`CallGraph::models`])
//! with throwaway diagnostics — every reader diagnostic was already reported,
//! with the callee's own file name in its spans, when the call resolved.

use std::collections::BTreeMap;

use frontend::FileSource;
use frontend::diag::{Diagnostics, Span};
use frontend::yaml::Document;

use crate::action::{
    ActionRef, ActionSource, ActionSourceError, PinnedAction, unavailable_hint,
};
use crate::model::{self, Workflow};

/// GitHub's nesting limit: a top-level workflow and up to three levels of
/// reusable workflows below it.
pub const MAX_DEPTH: usize = 3;

/// Where one workflow's text came from — and so, where its own `./` calls
/// resolve against.
#[derive(Clone)]
pub enum CalleeSource {
    /// The root file itself.
    Root,
    /// A repository-relative path in the root's repository.
    Local { path: String },
    /// A file of another repository at a pinned commit.
    Remote { pinned: PinnedAction },
}

/// One resolved callee: where it came from and its parsed document.
pub struct Callee {
    pub source: CalleeSource,
    pub doc: Document,
}

/// Every called workflow the root reaches, by identity.
#[derive(Default)]
pub struct CallGraph {
    resolved: BTreeMap<String, Callee>,
}

impl CallGraph {
    /// The callee a `uses:` names, resolved relative to the calling workflow's
    /// own source. `None` when resolution failed (already a diagnostic).
    pub fn callee(&self, caller: &CalleeSource, uses: &str) -> Option<(String, &Callee)> {
        let identity = target_of(caller, uses).ok()?.identity();
        self.resolved.get(&identity).map(|c| (identity, c))
    }

    /// The callee models, re-read from the owned documents with throwaway
    /// diagnostics — resolution already reported every reader finding.
    pub fn models(&self) -> BTreeMap<String, (CalleeSource, Workflow<'_>)> {
        let mut out = BTreeMap::new();
        for (identity, callee) in &self.resolved {
            let mut scratch = Diagnostics::new();
            if let Some(wf) = model::read(&callee.doc, &mut scratch) {
                out.insert(identity.clone(), (callee.source.clone(), wf));
            }
        }
        out
    }
}

/// A same-repository reference: `./path`, or GitHub's `$/path` shorthand for
/// "this repository at this ref".
fn same_repo(uses: &str) -> Option<&str> {
    uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/"))
}

/// What a `uses:` names before any fetch: the repository file to read, or the
/// reference to resolve. Its identity is the [`CallGraph`] key and the
/// diagnostic name — a local path repo-relative, a remote reference by its
/// display form — so a pinned callee shared by many callers resolves once.
enum CallTarget {
    Local { path: String },
    Remote { reference: ActionRef },
}

impl CallTarget {
    fn identity(&self) -> String {
        match self {
            CallTarget::Local { path } => path.clone(),
            CallTarget::Remote { reference } => reference.to_string(),
        }
    }
}

/// The target a `uses:` names, seen from `caller`.
fn target_of(caller: &CalleeSource, uses: &str) -> Result<CallTarget, String> {
    if let Some(rest) = same_repo(uses) {
        return Ok(match caller {
            CalleeSource::Root | CalleeSource::Local { .. } => CallTarget::Local {
                path: rest.to_string(),
            },
            // A remote callee's `./` call names a file of its own repository,
            // at the same pin.
            CalleeSource::Remote { pinned } => {
                let mut reference = pinned.reference.clone();
                reference.path = Some(rest.into());
                CallTarget::Remote { reference }
            }
        });
    }
    let reference = ActionRef::parse(uses).map_err(|e| e.to_string())?;
    if !reference
        .path
        .as_deref()
        .is_some_and(|p| p.ends_with(".yml") || p.ends_with(".yaml"))
    {
        return Err("a reusable-workflow reference names a `.yml` file: \
                    `owner/repo/.github/workflows/name.yml@ref`"
            .into());
    }
    Ok(CallTarget::Remote { reference })
}

/// Resolve every workflow the root's call jobs reach. Errors — an unfetchable
/// file, a cycle, a nest past [`MAX_DEPTH`], a callee that is not reusable —
/// are diagnostics; resolution continues past them so one pass reports
/// everything it can.
pub fn resolve(
    root: &Workflow<'_>,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    diags: &mut Diagnostics,
) -> CallGraph {
    let mut graph = CallGraph::default();
    let mut stack = Vec::new();
    walk(
        root,
        &CalleeSource::Root,
        files,
        actions,
        &mut graph,
        &mut stack,
        diags,
    );
    graph
}

fn walk(
    wf: &Workflow<'_>,
    source: &CalleeSource,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    graph: &mut CallGraph,
    stack: &mut Vec<String>,
    diags: &mut Diagnostics,
) {
    // The callee references leave the borrow of `wf` before any recursion.
    let calls: Vec<(String, Span)> = wf
        .jobs
        .iter()
        .filter_map(|j| j.call.as_ref().map(|c| c.uses.clone()))
        .collect();
    for (uses, span) in calls {
        let target = match target_of(source, &uses) {
            Ok(target) => target,
            Err(message) => {
                diags.error(
                    "gha.bad_call",
                    span.clone(),
                    format!("`uses: {uses}`: {message}"),
                );
                continue;
            }
        };
        let identity = target.identity();
        if stack.contains(&identity) {
            diags.error(
                "gha.workflow_cycle",
                span.clone(),
                format!(
                    "workflow call cycle: {} -> `{identity}`",
                    stack
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ),
            );
            continue;
        }
        if graph.resolved.contains_key(&identity) {
            continue;
        }
        if stack.len() >= MAX_DEPTH {
            diags.error(
                "gha.workflow_depth",
                span.clone(),
                format!(
                    "reusable workflows nest more than {MAX_DEPTH} deep at `{identity}`",
                ),
            );
            continue;
        }
        let Some((callee_source, text)) = fetch(&target, &identity, files, actions, &span, diags)
        else {
            continue;
        };
        let Some(doc) = Document::parse(&identity, &text, diags) else {
            continue;
        };
        // Read with real diagnostics — the one loud read of this callee; the
        // lowering's later re-read is quiet.
        let wf_callee = model::read(&doc, diags);
        if let Some(wf_callee) = &wf_callee {
            if wf_callee.call.is_none() {
                diags.error(
                    "gha.not_reusable",
                    span.clone(),
                    format!("`{identity}` has no `on.workflow_call`, so it cannot be called"),
                );
            }
            stack.push(identity.clone());
            walk(
                wf_callee,
                &callee_source,
                files,
                actions,
                graph,
                stack,
                diags,
            );
            stack.pop();
        }
        graph.resolved.insert(
            identity,
            Callee {
                source: callee_source,
                doc,
            },
        );
    }
}

/// One callee's text: the repository for a local path, the action source for a
/// pin — with the same rejection story remote actions have when the source
/// cannot serve it.
fn fetch(
    target: &CallTarget,
    identity: &str,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
    span: &Span,
    diags: &mut Diagnostics,
) -> Option<(CalleeSource, String)> {
    match target {
        CallTarget::Local { path } => match files.read(path) {
            Some(text) => Some((CalleeSource::Local { path: path.clone() }, text)),
            None => {
                diags.error(
                    "gha.bad_call",
                    span.clone(),
                    format!("no workflow at `./{path}` in this repository"),
                );
                None
            }
        },
        CallTarget::Remote { reference } => {
            let Some(actions) = actions else {
                diags.unsupported(
                    "action.remote",
                    span.clone(),
                    identity.to_string(),
                    "no action source is configured, so workflows from other repositories cannot be fetched",
                );
                return None;
            };
            let fetched = actions
                .resolve(reference)
                .and_then(|pinned| actions.file(&pinned).map(|text| (pinned, text)));
            match fetched {
                Ok((pinned, text)) => Some((CalleeSource::Remote { pinned }, text)),
                Err(ActionSourceError::Unavailable { reason, .. }) => {
                    diags.unsupported(
                        "action.remote",
                        span.clone(),
                        identity.to_string(),
                        &unavailable_hint(reason),
                    );
                    None
                }
                Err(other) => {
                    diags.error(
                        "action.unresolved",
                        span.clone(),
                        format!("`uses: {identity}`: {other}"),
                    );
                    None
                }
            }
        }
    }
}
