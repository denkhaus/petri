//! The checks a graph must pass before lowering makes sense: one start,
//! one exit, every node declared and reachable.

use std::collections::{HashSet, VecDeque};

use super::{Ctx, shape_of};
use crate::kinds::GOAL_CHECK_NODE;
use crate::model::{NodeDecl, Workflow};

/// The start and exit nodes a well-formed workflow has.
pub(super) struct Structure {
    pub(super) start: String,
    pub(super) exit:  String,
}

impl Ctx<'_> {
    /// The checks a graph must pass before lowering makes sense.
    pub(super) fn structure(&mut self, workflow: &Workflow) -> Option<Structure> {
        let mut ok = true;
        for node in &workflow.nodes {
            if !node.declared {
                self.diags.error(
                    "attractor.undeclared_node",
                    node.span.clone(),
                    format!("`{}` is named by an edge but never declared", node.id),
                );
                ok = false;
            }
        }
        if workflow.node(GOAL_CHECK_NODE).is_some() {
            self.diags.error(
                "attractor.reserved_node_id",
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
                "attractor.no_start",
                workflow.span.clone(),
                "the workflow has no start node (`shape=Mdiamond`, `type=start`, or an id of `start`)",
            );
            return None;
        }
        if starts.len() > 1 {
            self.diags.error(
                "attractor.multiple_starts",
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
                "attractor.no_exit",
                workflow.span.clone(),
                "the workflow has no exit node (`shape=Msquare`, `type=exit`, or an id of `exit`)",
            );
            return None;
        }
        if exits.len() > 1 {
            self.diags.error(
                "attractor.multiple_exits",
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
                    "attractor.start_has_incoming",
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
                    "attractor.exit_has_outgoing",
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
                    "attractor.unreachable_node",
                    node.span.clone(),
                    format!("`{}` is not reachable from the start node", node.id),
                );
                ok = false;
            }
        }
        if !seen.contains(exit.as_str()) {
            self.diags.error(
                "attractor.exit_unreachable",
                workflow.span.clone(),
                "the exit node is not reachable from the start node",
            );
            ok = false;
        }
        ok.then_some(Structure { start, exit })
    }
}
