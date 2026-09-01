//! Run-time planning for one manifest-backed action.

use std::collections::{BTreeMap, BTreeSet};

use frontend::diag::{Diagnostic, Diagnostics};
use frontend::yaml::Document;
use frontend::{FileSource, Lowered};
use ir::placeholder::{EXPR_PLACEHOLDER_KEY, SECRET_REF_KEY};
use ir::{
    Edge, EdgeId, Expr, ExprId, ExprTable, Graph, GraphBody, GraphFragment, Local, Node, NodeId,
    Routing, Scope, ScopeId, SelectGroup, SelectionPolicy, StepRef, Value,
};
use serde_json::{Map, json};
use smol_str::SmolStr;

use super::{RuntimeSite, lower_internal};
use crate::action::{
    ACTION_KIND, ActionLocation, ActionSource, DEFERRED_ACTION_KIND, DEFERRED_ACTION_RESULT_KIND,
    DOCKER_ACTION_KIND, PinnedAction, RUN_KIND,
};
use crate::composite::{self, Runs};
use crate::exprs::{node_record, status_fold};
use crate::model;
use crate::runners::RunnerMap;

/// The resolved caller values needed to plan one action at run time.
pub struct DeferredActionPlan {
    pub action:            ActionLocation,
    pub job_id:            String,
    pub start_node:        String,
    pub step_id:           String,
    pub result_name:       String,
    pub with:              Map<String, Value>,
    pub env:               Map<String, Value>,
    pub event:             Value,
    pub soft_fail:         bool,
    pub tolerates_failure: bool,
    pub timeout_minutes:   Option<String>,
    pub job_environment:   Option<String>,
    pub background:        Option<String>,
    pub matrix:            bool,
    pub in_expansion:      bool,
    pub needs:             BTreeMap<String, String>,
    pub depth:             usize,
    /// The materialized expansion suffix, without `#`.
    pub index:             Option<u32>,
}

/// The fragments one resolver saves: main work now and an optional post phase
/// for the job cleanup barrier.
pub struct PlannedDeferredAction {
    pub main:        GraphFragment,
    pub post:        Option<GraphFragment>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lower one action against manifests read from its run-time repository tree.
/// The root action expands now. Actions found inside it remain deferred.
pub fn plan_deferred_action(
    request: &DeferredActionPlan,
    files: &dyn FileSource,
    actions: Option<&dyn ActionSource>,
) -> Result<PlannedDeferredAction, Vec<Diagnostic>> {
    let action_path = request.action.directory();
    let mut inspect_diags = Diagnostics::new();
    let Some(doc) = composite::read_document(
        files,
        action_path,
        &frontend::Span::file("<deferred-action>"),
        &mut inspect_diags,
    ) else {
        return Err(inspect_diags.into_vec());
    };
    let Some(manifest) = composite::read_manifest(&doc, &mut inspect_diags) else {
        return Err(inspect_diags.into_vec());
    };
    let composite_outputs: Vec<String> = match &manifest.runs {
        Runs::Composite(action) => action
            .outputs
            .iter()
            .map(|(name, _)| name.clone())
            .collect(),
        Runs::Node(_) | Runs::Docker(_) => Vec::new(),
    };

    let workflow = synthetic_workflow(request, &composite_outputs);
    let mut parse_diags = Diagnostics::new();
    let Some(workflow_doc) = Document::parse("<deferred-action>.json", &workflow, &mut parse_diags)
    else {
        return Err(parse_diags.into_vec());
    };
    let Some(model) = model::read(&workflow_doc, &mut parse_diags) else {
        return Err(parse_diags.into_vec());
    };
    let runtime_site = RuntimeSite {
        job_id:       request.job_id.clone(),
        start_node:   request.start_node.clone(),
        matrix:       request.matrix,
        in_expansion: request.in_expansion,
        needs:        request.needs.clone(),
        depth:        request.depth,
    };
    let Lowered { graph, diagnostics } = lower_internal(
        &model,
        files,
        actions,
        &RunnerMap::builtin(),
        false,
        parse_diags,
        Some((request.job_id.clone(), request.step_id.clone())),
        Some(runtime_site),
    );
    let Some(mut graph) = graph else {
        return Err(diagnostics.into_vec());
    };

    rewrite_locations(&mut graph, &request.action)?;
    configure_runtime_channels(&mut graph, request);
    let (main, post) = extract_fragments(graph, request)?;
    Ok(PlannedDeferredAction {
        main,
        post,
        diagnostics: diagnostics.into_vec(),
    })
}

fn synthetic_workflow(request: &DeferredActionPlan, outputs: &[String]) -> String {
    let mut step = Map::new();
    step.insert("id".into(), json!(request.step_id));
    let directory = request.action.directory();
    step.insert(
        "uses".into(),
        json!(if directory.is_empty() {
            "./".to_string()
        } else {
            format!("./{directory}")
        }),
    );
    step.insert("if".into(), json!("always()"));
    if !request.with.is_empty() {
        step.insert("with".into(), encoded_values(&request.with, true));
    }
    if !request.env.is_empty() {
        step.insert("env".into(), encoded_values(&request.env, false));
    }
    if let Some(timeout) = &request.timeout_minutes {
        step.insert("timeout-minutes".into(), json!(timeout));
    }

    let mut job = Map::new();
    job.insert("runs-on".into(), json!("ubuntu-latest"));
    job.insert("steps".into(), Value::Array(vec![Value::Object(step)]));
    if !outputs.is_empty() {
        let values = outputs
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    json!(format!(
                        "${{{{ steps.{}.outputs.{name} }}}}",
                        request.step_id
                    )),
                )
            })
            .collect();
        job.insert("outputs".into(), Value::Object(values));
    }
    serde_json::to_string(&json!({
        "on": "push",
        "jobs": {
            request.job_id.clone(): Value::Object(job),
        }
    }))
    .expect("a synthetic workflow serializes")
}

fn encoded_values(values: &Map<String, Value>, stringify: bool) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(name, value)| {
                let encoded = value
                    .as_object()
                    .and_then(|map| map.get(SECRET_REF_KEY))
                    .and_then(Value::as_str)
                    .map_or_else(
                        || {
                            if stringify {
                                json!(match value {
                                    Value::String(text) => text.clone(),
                                    other => other.to_string(),
                                })
                            } else {
                                value.clone()
                            }
                        },
                        |secret| json!(format!("${{{{ secrets.{secret} }}}}")),
                    );
                (name.clone(), encoded)
            })
            .collect(),
    )
}

fn configure_runtime_channels(graph: &mut Graph, request: &DeferredActionPlan) {
    let public = format!("{}/{}", request.job_id, request.step_id);
    for node in &mut graph.body.nodes {
        let Value::Object(config) = &mut node.step.config else {
            continue;
        };
        if matches!(
            node.step.kind.as_ref(),
            RUN_KIND | ACTION_KIND | DOCKER_ACTION_KIND | DEFERRED_ACTION_KIND
        ) {
            if let Some(channel) = &request.job_environment {
                config.insert("job_environment".into(), json!(channel));
            }
            if node.name != format!("{public}/post")
                && let Some(channel) = &request.background
            {
                config.insert("background".into(), json!(channel));
            }
        }
        if request.soft_fail || request.tolerates_failure || request.background.is_some() {
            node.tolerates_failure = true;
        }
    }
}

fn rewrite_locations(graph: &mut Graph, root: &ActionLocation) -> Result<(), Vec<Diagnostic>> {
    let ActionLocation::Pinned(pinned) = root else {
        return Ok(());
    };
    for node in &mut graph.body.nodes {
        let Value::Object(config) = &mut node.step.config else {
            continue;
        };
        let target = match node.step.kind.as_ref() {
            ACTION_KIND | DEFERRED_ACTION_KIND => config.get_mut("action"),
            DOCKER_ACTION_KIND => config
                .get_mut("image")
                .and_then(Value::as_object_mut)
                .and_then(|image| image.get_mut("dockerfile"))
                .and_then(Value::as_object_mut)
                .and_then(|dockerfile| dockerfile.get_mut("action")),
            _ => None,
        };
        let Some(target) = target else { continue };
        let Ok(ActionLocation::Local { local }) = serde_json::from_value(target.clone()) else {
            continue;
        };
        *target = serde_json::to_value(pinned_at(pinned, &local)).map_err(|error| {
            vec![Diagnostic::error(
                "gha.bad_action_path",
                frontend::Span::file("<deferred-action>"),
                error.to_string(),
            )]
        })?;
    }
    Ok(())
}

fn pinned_at(root: &PinnedAction, path: &str) -> ActionLocation {
    ActionLocation::Pinned(
        root.at_repository_path(path)
            .expect("the local action path was validated while parsing"),
    )
}

fn extract_fragments(
    mut graph: Graph,
    request: &DeferredActionPlan,
) -> Result<(GraphFragment, Option<GraphFragment>), Vec<Diagnostic>> {
    let public = format!("{}/{}", request.job_id, request.step_id);
    let start = format!("{}/start", request.job_id);
    let done = format!("{}/done", request.job_id);
    let post = format!("{public}/post");

    let done_id = graph
        .nodes
        .iter()
        .find(|node| node.name == done)
        .map(|node| node.id)
        .ok_or_else(|| planner_error("the synthetic action job has no terminal"))?;
    let summary = graph
        .nodes
        .iter()
        .flat_map(|node| node.routing.edges().map(move |edge| (node, edge)))
        .find(|(_, edge)| edge.to == done_id)
        .and_then(|(_, edge)| edge.map);
    let composite_output = summary.and_then(|id| summary_outputs(&graph.exprs, id));

    let mut name_map = BTreeMap::new();
    for node in &graph.body.nodes {
        if node.name == start || node.name == done {
            continue;
        }
        name_map.insert(
            node.name.to_string(),
            runtime_base_name(&node.name, &public),
        );
    }
    graph.body.exprs = retag_exprs_to_live(&graph.exprs, &name_map);

    let post_ids: BTreeSet<NodeId> = graph
        .nodes
        .iter()
        .filter(|node| node.name == post)
        .map(|node| node.id)
        .collect();
    let main_ids: BTreeSet<NodeId> = graph
        .nodes
        .iter()
        .filter(|node| node.name != start && node.name != done)
        .filter(|node| !post_ids.contains(&node.id))
        .map(|node| node.id)
        .collect();
    if main_ids.is_empty() {
        return Err(planner_error("the action manifest produced no main step"));
    }

    let mut exprs = retag_exprs(&graph.exprs);
    let post_exprs = (!post_ids.is_empty()).then(|| exprs.clone());
    let expanded = request.matrix || request.in_expansion;
    let result_names: Vec<String> = graph
        .nodes
        .iter()
        .filter(|node| main_ids.contains(&node.id))
        .filter_map(|node| name_map.get(node.name.as_str()).cloned())
        .collect();
    let status = status_fold(&mut exprs, &result_names, expanded);
    let output = if let Some(main) = graph.nodes.iter().find(|node| node.name == public) {
        let name = name_map
            .get(main.name.as_str())
            .expect("the main action node has a runtime name");
        let record = node_record(&mut exprs, name, expanded);
        exprs.field(record, "output")
    } else {
        composite_output.map_or_else(|| exprs.object(Vec::new()), |id| ExprId::new(id.raw()))
    };

    let main = make_main_fragment(&graph, &main_ids, &name_map, exprs, request, status, output)?;
    let post = match post_exprs {
        None => None,
        Some(exprs) => Some(make_fragment(
            &graph,
            &post_ids,
            &name_map,
            exprs,
            request.index,
        )?),
    };
    Ok((main, post))
}

fn make_main_fragment(
    graph: &Graph,
    ids: &BTreeSet<NodeId>,
    names: &BTreeMap<String, String>,
    exprs: ExprTable<Local>,
    request: &DeferredActionPlan,
    status: ExprId<Local>,
    output: ExprId<Local>,
) -> Result<GraphFragment, Vec<Diagnostic>> {
    let mut fragment = make_fragment(graph, ids, names, exprs, request.index)?;
    let result_id = NodeId::new(
        u32::try_from(fragment.nodes.len()).expect("a fragment never exceeds u32::MAX nodes"),
    );
    let result_name = materialized_name(&request.result_name, request.index);
    let mut result = Node::new(
        result_id,
        &result_name,
        ScopeId::new(0),
        StepRef::new(
            DEFERRED_ACTION_RESULT_KIND,
            json!({
                "status": { EXPR_PLACEHOLDER_KEY: status.raw() },
                "output": { EXPR_PLACEHOLDER_KEY: output.raw() },
            }),
        ),
    );
    result.run_on_cancel = true;
    result.tolerates_failure = true;

    let terminals: Vec<NodeId<Local>> = fragment
        .nodes
        .iter()
        .filter(|node| node.routing.edges().next().is_none())
        .map(|node| node.id)
        .collect();
    let edge_base = fragment
        .nodes
        .iter()
        .flat_map(|node| node.routing.edges().map(|edge| edge.id.raw()))
        .max()
        .map_or(0, |id| id + 1);
    for (offset, terminal) in terminals.into_iter().enumerate() {
        let offset = u32::try_from(offset).expect("a fragment never exceeds u32::MAX edges");
        fragment.body.nodes[terminal.index()].routing =
            Routing::next(Edge::always(EdgeId::new(edge_base + offset), result_id));
    }
    fragment.body.nodes.push(result);
    fragment.exits = vec![result_id];
    Ok(fragment)
}

fn make_fragment(
    graph: &Graph,
    ids: &BTreeSet<NodeId>,
    names: &BTreeMap<String, String>,
    exprs: ExprTable<Local>,
    index: Option<u32>,
) -> Result<GraphFragment, Vec<Diagnostic>> {
    let remap: BTreeMap<NodeId, NodeId<Local>> = ids
        .iter()
        .enumerate()
        .map(|(next, old)| {
            (
                *old,
                NodeId::new(u32::try_from(next).expect("a fragment never exceeds u32::MAX nodes")),
            )
        })
        .collect();
    let mut incoming = BTreeSet::new();
    let mut nodes = Vec::new();
    let mut next_edge = 0u32;
    for old in ids {
        let source = &graph.nodes[old.index()];
        let mut live = source.clone();
        live.id = NodeId::new(remap[old].raw());
        live.name = SmolStr::new(materialized_name(
            names
                .get(source.name.as_str())
                .ok_or_else(|| planner_error("an action node has no runtime name"))?,
            index,
        ));
        live.scope = ScopeId::new(0);
        live.expand = None;
        let mut groups = Vec::new();
        for group in &source.routing.groups {
            let mut edge_ids = BTreeMap::new();
            let mut arms = Vec::new();
            for arm in &group.arms {
                let Some(&target) = remap.get(&arm.to) else {
                    continue;
                };
                incoming.insert(target);
                let edge_id = EdgeId::new(next_edge);
                next_edge += 1;
                edge_ids.insert(arm.id, edge_id);
                arms.push(Edge {
                    id: edge_id,
                    to: NodeId::new(target.raw()),
                    ..arm.clone()
                });
            }
            if arms.is_empty() {
                continue;
            }
            let policy = match &group.policy {
                SelectionPolicy::FirstMatch => SelectionPolicy::FirstMatch,
                SelectionPolicy::Tiered(tiers) => SelectionPolicy::Tiered(
                    tiers
                        .iter()
                        .map(|tier| ir::Tier {
                            candidates: tier
                                .candidates
                                .iter()
                                .filter_map(|candidate| {
                                    edge_ids.get(&candidate.edge).map(|edge| ir::Candidate {
                                        edge: *edge,
                                        ..*candidate
                                    })
                                })
                                .collect(),
                            pick:       tier.pick,
                        })
                        .collect(),
                ),
            };
            groups.push(SelectGroup {
                policy,
                arms,
                fallthrough: group.fallthrough,
            });
        }
        live.routing = Routing { groups };
        let local: Node<Local> =
            serde_json::from_value(serde_json::to_value(live).expect("an action node serializes"))
                .expect("id-space markers do not change the node wire format");
        nodes.push(local);
    }
    let entries = remap
        .values()
        .filter(|id| !incoming.contains(id))
        .copied()
        .collect();
    let exits = nodes
        .iter()
        .filter(|node| node.routing.edges().next().is_none())
        .map(|node| node.id)
        .collect();
    Ok(GraphFragment {
        body: GraphBody {
            nodes,
            scopes: vec![Scope::new(ScopeId::new(0))],
            exprs,
            entry: entries,
        },
        exits,
    })
}

fn runtime_base_name(name: &str, public: &str) -> String {
    name.strip_prefix(public).map_or_else(
        || name.to_string(),
        |suffix| format!("{public}/runtime{suffix}"),
    )
}

fn materialized_name(base: &str, index: Option<u32>) -> String {
    index.map_or_else(|| base.to_string(), |index| format!("{base}#{index}"))
}

fn retag_exprs_to_live(source: &ExprTable, names: &BTreeMap<String, String>) -> ExprTable {
    let mut out = ExprTable::new();
    for (_, expr) in source.iter() {
        out.push(rewrite_expr(expr, names));
    }
    out
}

fn retag_exprs(source: &ExprTable) -> ExprTable<Local> {
    serde_json::from_value(serde_json::to_value(source).expect("expressions serialize"))
        .expect("id-space markers do not change the expression wire format")
}

fn rewrite_expr(expr: &Expr, names: &BTreeMap<String, String>) -> Expr {
    let rewrite = |value: &str| {
        names.get(value).cloned().or_else(|| {
            value
                .strip_suffix('#')
                .and_then(|base| names.get(base).map(|replacement| format!("{replacement}#")))
        })
    };
    match expr {
        Expr::Lit(Value::String(value)) => Expr::Lit(Value::String(
            rewrite(value).unwrap_or_else(|| value.clone()),
        )),
        Expr::Field(base, field) => Expr::Field(
            *base,
            rewrite(field).map_or_else(|| field.clone(), SmolStr::new),
        ),
        other => other.clone(),
    }
}

fn summary_outputs(table: &ExprTable, summary: ExprId) -> Option<ExprId> {
    let Expr::Object(fields) = table.get(summary)? else {
        return None;
    };
    fields
        .iter()
        .find(|(name, _)| name == "outputs")
        .map(|(_, id)| *id)
}

fn planner_error(message: &str) -> Vec<Diagnostic> {
    vec![Diagnostic::error(
        "gha.deferred_action",
        frontend::Span::file("<deferred-action>"),
        message,
    )]
}
