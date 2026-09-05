//! Compile each workflow as a graph, reusing equivalent child bindings.

use std::collections::{BTreeMap, HashMap};
use std::mem;

use frontend::diag::{Diagnostic, Diagnostics, Lowered};
use frontend::{FileSource, graph_digest};
use ir::{Graph, GraphBuilder, Value};

use super::{ActionResolutions, Lowering, LoweringMode, WorkflowContext};
use crate::action::{ActionSource, CHECKOUT_KIND, REPO_PARAM_CONTEXT, WORKFLOW_CALL_KIND};
use crate::call::{CallGraph, CalleeSource};
use crate::exprs::{SecretMap, undeclared_secret, whole_value_secret};
use crate::model::{CallInterface, InputDecl, SecretsArg, Workflow, WorkflowCall};
use crate::runners::RunnerMap;
use crate::{identity, inputs};

#[derive(Clone, PartialEq)]
struct WorkflowBindings {
    static_inputs: BTreeMap<String, Value>,
    secrets:       SecretMap,
}

#[derive(Clone)]
pub(super) struct CompiledWorkflow {
    pub digest:     String,
    pub needs_repo: bool,
}

pub(super) struct CallPlan {
    pub child:   CompiledWorkflow,
    pub inputs:  inputs::BoundInputs,
    pub secrets: Option<BTreeMap<String, Option<String>>>,
}

pub(super) struct Compiler<'w, 'a> {
    calls:               &'w CallGraph,
    models:              &'w BTreeMap<String, (CalleeSource, Workflow<'a>)>,
    files:               &'w dyn FileSource,
    actions:             Option<&'w dyn ActionSource>,
    runners:             &'w RunnerMap,
    substitute_checkout: bool,
    resolved:            &'w ActionResolutions,
    github_identity:     Value,
    cache:               HashMap<String, Vec<(WorkflowBindings, CompiledWorkflow)>>,
    children:            BTreeMap<String, Graph>,
}

impl<'w, 'a> Compiler<'w, 'a> {
    pub(super) fn new(
        calls: &'w CallGraph,
        models: &'w BTreeMap<String, (CalleeSource, Workflow<'a>)>,
        files: &'w dyn FileSource,
        actions: Option<&'w dyn ActionSource>,
        runners: &'w RunnerMap,
        substitute_checkout: bool,
        resolved: &'w ActionResolutions,
    ) -> Self {
        Self {
            calls,
            models,
            files,
            actions,
            runners,
            substitute_checkout,
            resolved,
            github_identity: identity::github_context(identity::repository_slug(files).as_deref()),
            cache: HashMap::new(),
            children: BTreeMap::new(),
        }
    }

    pub(super) fn lower_root(
        mut self,
        wf: &'w Workflow<'a>,
        diags: Diagnostics,
        mode: LoweringMode,
    ) -> Lowered {
        let mut lowered = self.lower_one(wf, CalleeSource::Root, None, diags, mode);
        if lowered.graph.is_some() {
            lowered.children = self.children.into_values().collect();
        }
        lowered
    }

    fn callee(
        &self,
        source: &CalleeSource,
        uses: &str,
    ) -> Option<(String, &'w CalleeSource, &'w Workflow<'a>)> {
        let (identity, _) = self.calls.callee(source, uses)?;
        let (source, workflow) = self.models.get(&identity)?;
        Some((identity, source, workflow))
    }

    fn lower_child(
        &mut self,
        identity: String,
        source: CalleeSource,
        wf: &'w Workflow<'a>,
        bindings: WorkflowBindings,
        diags: &mut Diagnostics,
    ) -> Option<CompiledWorkflow> {
        if let Some((_, child)) = self
            .cache
            .get(&identity)
            .and_then(|entries| entries.iter().find(|(key, _)| key == &bindings))
        {
            return Some(child.clone());
        }
        let lowered = self.lower_one(
            wf,
            source,
            Some(&bindings),
            Diagnostics::new(),
            LoweringMode::Deferred,
        );
        for diagnostic in lowered.diagnostics.into_vec() {
            diags.push(diagnostic);
        }
        let graph = lowered.graph?;
        let child = CompiledWorkflow {
            digest:     graph_digest(&graph),
            needs_repo: graph.nodes.iter().any(|node| {
                node.step.kind.as_ref() == CHECKOUT_KIND
                    || (node.step.kind.as_ref() == WORKFLOW_CALL_KIND
                        && node.step.config["context"]["parameters"]
                            .get(REPO_PARAM_CONTEXT)
                            .is_some())
            }),
        };
        self.children.entry(child.digest.clone()).or_insert(graph);
        self.cache
            .entry(identity)
            .or_default()
            .push((bindings, child.clone()));
        Some(child)
    }

    fn lower_one(
        &mut self,
        wf: &'w Workflow<'a>,
        source: CalleeSource,
        bindings: Option<&WorkflowBindings>,
        diags: Diagnostics,
        mode: LoweringMode,
    ) -> Lowered {
        let is_invocation = bindings.is_some()
            || matches!(&mode, LoweringMode::Runtime { site, .. } if site.invocation);
        let mut lw = Lowering {
            b: GraphBuilder::bare(),
            diags,
            wf,
            source,
            context: WorkflowContext::default(),
            is_invocation,
            files: self.files,
            actions: self.actions,
            runners: self.runners,
            substitute_checkout: self.substitute_checkout,
            resolved: self.resolved,
            jobs: HashMap::new(),
            spans: HashMap::new(),
            leg_runs_on: None,
            github_identity: self.github_identity.clone(),
            mode,
        };
        lw.bind_workflow_context(bindings);
        for job in &wf.jobs {
            if let Some((_, source, _)) = job
                .call
                .as_ref()
                .and_then(|call| self.callee(&lw.source, &call.uses.0))
            {
                lw.call_shell(job, source);
            } else {
                lw.job_shell(job);
            }
        }
        for job in &wf.jobs {
            let Some(call) = &job.call else {
                lw.job_body(job);
                continue;
            };
            let Some((identity, source, callee)) = self.callee(&lw.source, &call.uses.0) else {
                continue;
            };
            let site = lw.base_site(job);
            let interface = callee.call.as_ref();
            let decls = interface.map_or(&[][..], |interface| interface.inputs.as_slice());
            let inputs = inputs::bind_call_inputs(
                decls,
                &call.with,
                &site,
                &call.uses.0,
                &call.uses.1,
                lw.b.exprs(),
                &mut lw.diags,
            );
            let bound_secrets = lw.bind_secrets(call, interface, &site.secrets);
            let child_secrets = match &bound_secrets {
                SecretMap::Inherit => SecretMap::Inherit,
                SecretMap::Explicit(map) => SecretMap::Explicit(
                    map.iter()
                        .map(|(name, parent)| (name.clone(), parent.as_ref().map(|_| name.clone())))
                        .collect(),
                ),
            };
            let bindings = WorkflowBindings {
                static_inputs: inputs.statics.clone(),
                secrets:       child_secrets,
            };
            let Some(child) =
                self.lower_child(identity, source.clone(), callee, bindings, &mut lw.diags)
            else {
                continue;
            };
            let secrets = if matches!(call.secrets, SecretsArg::Inherit) {
                None
            } else {
                let SecretMap::Explicit(mut map) = bound_secrets else {
                    unreachable!("an explicit call has an explicit secret map")
                };
                map.insert("GITHUB_TOKEN".into(), Some("GITHUB_TOKEN".into()));
                Some(map)
            };
            lw.call_body(job, CallPlan {
                child,
                inputs,
                secrets,
            });
        }
        let summary = bindings.map(|_| lw.workflow_summary());
        for job in &wf.jobs {
            lw.job_edges(job, summary);
        }
        lw.finish_graph()
    }
}

impl Lowering<'_, '_> {
    fn bind_workflow_context(&mut self, bindings: Option<&WorkflowBindings>) {
        if let Some(bindings) = bindings {
            let t = self.b.exprs();
            let kv = t.var("kv");
            let inputs = t.field(kv, "inputs");
            let exprs = self
                .wf
                .call
                .iter()
                .flat_map(|interface| &interface.inputs)
                .map(|decl| (decl.name.clone(), t.field(inputs, &decl.name)))
                .collect();
            self.context = WorkflowContext {
                inputs:        Some(exprs),
                static_inputs: bindings.static_inputs.clone(),
                secrets:       bindings.secrets.clone(),
            };
            return;
        }
        let mut decls: Vec<&InputDecl<'_>> = self
            .wf
            .call
            .iter()
            .flat_map(|interface| &interface.inputs)
            .collect();
        decls.extend(self.wf.dispatch_inputs.iter());
        if !decls.is_empty() {
            let bound = inputs::bind_param_inputs(&decls, self.b.exprs(), &mut self.diags);
            self.context.inputs = Some(bound.exprs);
            self.context.static_inputs = bound.statics;
        }
    }

    fn finish_graph(mut self) -> Lowered {
        if self.diags.has_errors() {
            return Lowered::rejected(self.diags);
        }
        let builder = mem::replace(&mut self.b, GraphBuilder::bare());
        let mut graph = builder.build();
        graph.normalize_loop_heads();
        let report = ir::check(&graph);
        for error in &report.errors {
            let span = error
                .primary_node()
                .and_then(|node| self.spans.get(&node).cloned())
                .unwrap_or_else(|| self.wf.span.clone());
            let mut diagnostic = Diagnostic::error(error.code(), span, error.to_string());
            if let Some(hint) = error.hint() {
                diagnostic = diagnostic.with_hint(hint);
            }
            self.diags.push(diagnostic);
        }
        for warning in &report.warnings {
            let span = self
                .spans
                .get(&warning.primary_node())
                .cloned()
                .unwrap_or_else(|| self.wf.span.clone());
            let mut diagnostic = Diagnostic::warning(warning.code(), span, warning.to_string());
            if let Some(hint) = warning.hint() {
                diagnostic = diagnostic.with_hint(hint);
            }
            self.diags.push(diagnostic);
        }
        Lowered::from_parts(graph, self.diags)
    }
}

impl<'a> Lowering<'_, 'a> {
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
        let declared: Vec<(String, bool)> =
            interface.map(|i| i.secrets.clone()).unwrap_or_default();
        let provided = match &call.secrets {
            SecretsArg::Inherit => {
                let mut inherited = caller_secrets.clone();
                if let SecretMap::Explicit(map) = &mut inherited {
                    for (name, required) in &declared {
                        let entry = map.entry(name.to_lowercase()).or_insert(None);
                        if *required && entry.is_none() {
                            self.diags.error(
                                "gha.missing_secret",
                                call.uses.1.clone(),
                                format!("`{}` requires secret `{name}`", call.uses.0),
                            );
                        }
                    }
                }
                return inherited;
            }
            SecretsArg::None => Vec::new(),
            SecretsArg::Map(entries) => entries.clone(),
        };
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
                Some(provider) => {
                    if let Ok(entry) = caller_secrets.resolve(&provider) {
                        entry
                    } else {
                        undeclared_secret(&mut self.diags, node.span(), &provider);
                        None
                    }
                }
            };
            map.insert(lowered, entry);
        }
        SecretMap::Explicit(map)
    }
}
