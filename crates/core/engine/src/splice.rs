//! The one splice applicator.
//!
//! Every live-graph mutation flows through [`PreparedSplice`]: engine-private,
//! non-serializable, constructible only by a producer that finished preparing —
//! `apply_prepared_splice` is infallible for domain errors, because validation is
//! a type boundary here, not a convention. `ForEach` expansion and outcome
//! splices are the two producers; the applicator stays producer-neutral, so
//! producer-only behavior (supersession, seeding, retraction) rides in as data
//! on the shared [`SpliceProducer`] shape that also lands in the
//! [`AppliedSplice`] record.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ir::{CancelScopeId, EdgeId, Generation, Node, NodeId, Token, Value};
use smol_str::SmolStr;

use crate::event::Event;
use crate::state::{AppliedSplice, EngineState, SpliceBatchId, SpliceProducer};

/// One seed for a spliced entry: the synthetic incoming edge, the node it feeds,
/// and the token placed on it.
pub(crate) struct PreparedSeed {
    pub edge: EdgeId,
    pub entry: NodeId,
    pub generation: Generation,
    pub payload: Value,
}

/// A splice with every identifier remapped and every check complete: the only
/// input the applicator accepts.
pub(crate) struct PreparedSplice {
    pub batch: SpliceBatchId,
    /// The node whose firing produced this batch: the expansion source, or the
    /// uploader. Stamped on the record — ownership is core-derived.
    pub owner: NodeId,
    pub cancel_scope: CancelScopeId,
    /// The scope the fresh batch scope nests under: the producing firing's
    /// current cancel scope.
    pub parent_scope: CancelScopeId,
    /// Fully formed live-space nodes, ids contiguous with the live graph.
    pub nodes: Vec<Node>,
    /// `item` / `index` bindings for expansion clones; empty for uploads.
    pub bindings: BTreeMap<NodeId, BTreeMap<SmolStr, Value>>,
    /// Seed tokens for entries fed by no real edge (`ForEach` clones). Uploaded
    /// fragments attach through real select groups instead and seed nothing.
    pub seeds: Vec<PreparedSeed>,
    /// Producer-only behavior, applied here and recorded verbatim on the
    /// [`AppliedSplice`].
    pub producer: SpliceProducer,
}

/// Apply one prepared splice: mutate the live graph, record the batch, seed the
/// entries. Infallible — everything that can be refused was refused during
/// preparation, before this type could exist.
pub(crate) fn apply_prepared_splice(
    state: &mut EngineState,
    prepared: PreparedSplice,
    queue: &mut VecDeque<Event>,
) {
    let mut batch_nodes = BTreeSet::new();
    for node in &prepared.nodes {
        // Ids were allocated against this exact length, so a mismatch means the
        // splice was built against a different graph.
        debug_assert_eq!(node.id.index(), state.graph.nodes.len());
        state.graph.nodes.push(node.clone());
        if let Some(bindings) = prepared.bindings.get(&node.id) {
            state.set_clone_bindings(node.id, bindings.clone());
        }
        state.set_node_cancel_scope(node.id, prepared.cancel_scope);
        batch_nodes.insert(node.id);
    }
    for seed in &prepared.seeds {
        state.register_seed_edge(seed.edge, seed.entry);
    }

    state.add_cancel_scope(prepared.cancel_scope, prepared.parent_scope, batch_nodes.clone());

    match &prepared.producer {
        SpliceProducer::ForEach { superseded, .. } => {
            for node in superseded {
                state.supersede(*node);
            }
        }
        SpliceProducer::Outcome { retracted } => {
            state.retract_admissions(retracted);
        }
    }

    state.push_splice(AppliedSplice {
        batch: prepared.batch,
        owner: prepared.owner,
        nodes: batch_nodes,
        cancel_scope: prepared.cancel_scope,
        producer: prepared.producer,
    });

    for seed in &prepared.seeds {
        queue.push_back(Event::TokenEmitted(Token::seeded(
            seed.edge,
            seed.generation,
            seed.payload.clone(),
        )));
    }
}
