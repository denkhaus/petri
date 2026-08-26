//! `Node.meta`: opaque, host-facing metadata the engine carries and never reads.

use ir::{Graph, GraphBuilder, ScopeId, StepKindId, Value};
use serde_json::json;

const NOOP: StepKindId = StepKindId::new_static("noop");

fn linear() -> Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let finish = b.add_step("finish", scope, NOOP);
    b.link(start, finish);
    b.build()
}

/// A graph serialized before `meta` existed still deserializes: the field defaults
/// to `Null` and is skipped when null, so an unannotated graph round-trips to the
/// same bytes it always had.
#[test]
fn a_graph_without_meta_deserializes_to_null() {
    let graph = linear();
    let encoded = serde_json::to_string(&graph).expect("encode");
    assert!(
        !encoded.contains("\"meta\""),
        "null meta is skipped on the wire"
    );
    let decoded: Graph = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, graph);
    assert!(decoded.nodes.iter().all(|n| n.meta.is_null()));
}

/// Meta survives a round trip untouched: the engine carries it, nothing reads it.
#[test]
fn meta_round_trips() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let build = b.add_step("build", scope, NOOP);
    b.set_meta(
        build,
        json!({ "label": "Build the site", "span": { "line": 12, "col": 3 } }),
    );
    let graph = b.build();

    let encoded = serde_json::to_string(&graph).expect("encode");
    let decoded: Graph = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, graph);
    assert_eq!(
        decoded.nodes[build.index()].meta["label"],
        Value::from("Build the site")
    );
}
