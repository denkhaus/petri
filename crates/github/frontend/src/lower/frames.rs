//! The flat plan: the call graph flattened into frames and materialized job
//! entries, breadth first, and each frame's context bound top-down.

use std::borrow::Cow;
use std::collections::BTreeMap;

use frontend::diag::{Diagnostics, Span};

use crate::call::{self, CallGraph, CalleeSource};
use crate::exprs::{SEP, SecretMap, undeclared_secret, whole_value_secret};
use crate::inputs;
use crate::model::{CallInterface, Job, SecretsArg, Workflow, WorkflowCall};

use super::{CallEdge, Entry, EntryKind, Frame, FrameCtx, Lowering};

/// A job under its frame's prefix: the id and every `needs` entry prefixed, so
/// names, `JobNodes` keys and wiring stay collision-free across inlined
/// workflows. The as-written names live on in each `Site`, which strips the
/// prefix for the `needs.*` context. With no prefix — the root frame, so the
/// common case — the job is borrowed, not cloned.
fn materialize<'w, 'a>(job: &'w Job<'a>, prefix: &str) -> Cow<'w, Job<'a>> {
    if prefix.is_empty() {
        return Cow::Borrowed(job);
    }
    let mut out = job.clone();
    out.id = format!("{prefix}{SEP}{}", job.id);
    out.needs = job
        .needs
        .iter()
        .map(|(need, span)| (format!("{prefix}{SEP}{need}"), span.clone()))
        .collect();
    Cow::Owned(out)
}

/// Flatten the call graph into frames and materialized job entries, breadth
/// first from the root. A call whose callee failed to resolve (already a
/// diagnostic) becomes a plain empty job so its `needs` wiring stays intact
/// while the errors reject the workflow.
pub(super) fn plan<'w, 'a>(
    frames: &mut Vec<Frame<'w, 'a>>,
    entries: &mut Vec<Entry<'w, 'a>>,
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
            if has_matrix && in_expansion {
                nested_matrix(diags, &job.span);
            }
            let callee = job.call.as_ref().and_then(|call| {
                (depth < call::MAX_DEPTH)
                    .then(|| calls.callee(&source, &call.uses.0))
                    .flatten()
                    .and_then(|(identity, _)| models.get(&identity))
            });
            let kind = match callee {
                Some((callee_source, callee_wf)) => {
                    frames.push(Frame {
                        wf: callee_wf,
                        source: callee_source.clone(),
                        prefix: materialized.id.clone(),
                        call: Some(CallEdge {
                            caller: i,
                            entry: entries.len(),
                        }),
                        in_expansion: in_expansion || has_matrix,
                        depth: depth + 1,
                    });
                    EntryKind::Call {
                        callee: frames.len() - 1,
                    }
                }
                None => EntryKind::Job,
            };
            entries.push(Entry {
                frame: i,
                job: materialized,
                kind,
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

impl<'w, 'a> Lowering<'w, 'a> {
    /// Bind one frame's context: the root's inputs come from the run's
    /// parameters, a callee's from its call site — `with:` lowered in the
    /// caller's own site, `secrets:` folded through the caller's map so a
    /// nested `inherit` keeps renames intact.
    pub(super) fn bind_frame(&mut self, i: usize, entries: &[Entry<'w, 'a>]) {
        let frame_wf = self.frames[i].wf;
        let Some(CallEdge { caller, entry }) = self.frames[i].call else {
            // The root: `workflow_call` and `workflow_dispatch` declarations
            // both bind from run parameters, through one typed path.
            let mut decls: Vec<&crate::model::InputDecl<'_>> = Vec::new();
            if let Some(interface) = &frame_wf.call {
                decls.extend(interface.inputs.iter());
            }
            decls.extend(frame_wf.dispatch_inputs.iter());
            let bound = (!decls.is_empty())
                .then(|| inputs::bind_param_inputs(&decls, self.b.exprs(), &mut self.diags));
            self.frame_ctx[i] = match bound {
                Some(bound) => FrameCtx {
                    inputs: Some(bound.exprs),
                    static_inputs: bound.statics,
                    ..Default::default()
                },
                None => FrameCtx::default(),
            };
            return;
        };
        let frame_remote = matches!(self.frames[i].source, CalleeSource::Remote { .. });
        let call_job = &entries[entry].job;
        let call = call_job
            .call
            .as_ref()
            .expect("a callee frame's entry is a call");
        // Bind in the caller's context.
        self.enter(caller);
        let caller_site = self.base_site(call_job);
        let interface = frame_wf.call.as_ref();
        let decls = interface.map(|i| i.inputs.as_slice()).unwrap_or(&[]);
        let bound = inputs::bind_call_inputs(
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
            inputs: Some(bound.exprs),
            static_inputs: bound.statics,
            secrets,
            call_start: Some(format!("{}{SEP}start", call_job.id)),
            exit: None,
            remote: frame_remote || self.frame_ctx[caller].remote,
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
        let declared: Vec<(String, bool)> =
            interface.map(|i| i.secrets.clone()).unwrap_or_default();
        for (name, node) in &provided {
            if !declared
                .iter()
                .any(|(d, _)| d.to_lowercase() == name.to_lowercase())
            {
                self.diags.error(
                    "gha.bad_call",
                    node.span(),
                    format!("secret `{name}` is not declared by the called workflow"),
                );
            }
        }
        let mut map: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (name, required) in &declared {
            let lowered = name.to_lowercase();
            let value = provided
                .iter()
                .find(|(k, _)| k.to_lowercase() == lowered)
                .map(|(_, v)| *v);
            let Some(node) = value else {
                if *required {
                    self.diags.error(
                        "gha.missing_secret",
                        call.uses.1.clone(),
                        format!("`{}` requires secret `{name}`", call.uses.0),
                    );
                } else {
                    map.insert(lowered, None);
                }
                continue;
            };
            let entry = match whole_value_secret(node.as_str().unwrap_or("")) {
                None => {
                    self.diags.error(
                        "gha.bad_call",
                        node.span(),
                        format!(
                            "secret `{name}` must be a whole `${{{{ secrets.NAME }}}}` reference"
                        ),
                    );
                    None
                }
                Some(provider) => match caller_secrets.resolve(&provider) {
                    Ok(entry) => entry,
                    Err(crate::exprs::UndeclaredSecret) => {
                        undeclared_secret(&mut self.diags, node.span(), &provider);
                        None
                    }
                },
            };
            map.insert(lowered, entry);
        }
        SecretMap::Explicit(map)
    }
}
