//! Stage-1 splice vocabulary: the policy order, fragment validation through the
//! shared invariant engine, and the serialized shapes the log version rides on.

use ir::{
    Attachment, Edge, EdgeId, ExistingNodeRef, FragmentErrorKind, GraphFragment, Local, Node,
    NodeId, Outcome, ReplaceScope, Routing, Scope, ScopeId, SpliceMode, SplicePolicy,
    SpliceRequest, StepRef, ValidationError, validate_fragment, validate_request,
};
use serde_json::json;

fn local_node(id: u32, name: &str) -> Node<Local> {
    Node::new(
        NodeId::new(id),
        name,
        ScopeId::new(0),
        StepRef::new("noop", serde_json::Value::Null),
    )
}

fn one_node_fragment() -> GraphFragment {
    GraphFragment {
        nodes: vec![local_node(0, "uploaded")],
        scopes: vec![Scope::new(ScopeId::new(0))],
        exprs: Default::default(),
        entries: vec![NodeId::new(0)],
        exits: vec![NodeId::new(0)],
    }
}

// ── Policy order ──────────────────────────────────────────────────────────

#[test]
fn the_policy_order_is_total_and_exactly_as_specified() {
    let ladder = [
        SplicePolicy::Deny,
        SplicePolicy::Append,
        SplicePolicy::Replace {
            scope: ReplaceScope::OwnBatches,
        },
        SplicePolicy::Replace {
            scope: ReplaceScope::AllPending,
        },
    ];
    for pair in ladder.windows(2) {
        assert!(pair[0] < pair[1], "{:?} < {:?}", pair[0], pair[1]);
    }
}

#[test]
fn authorize_rejects_an_operation_above_the_policy_and_nothing_below() {
    let append = SpliceMode::Append;
    let own = SpliceMode::Replace {
        scope: ReplaceScope::OwnBatches,
    };
    let all = SpliceMode::Replace {
        scope: ReplaceScope::AllPending,
    };

    assert!(!SplicePolicy::Deny.authorizes(&append));
    assert!(SplicePolicy::Append.authorizes(&append));
    assert!(!SplicePolicy::Append.authorizes(&own));
    // A higher policy authorizes everything beneath it: the order is authority.
    let own_policy = SplicePolicy::Replace {
        scope: ReplaceScope::OwnBatches,
    };
    assert!(own_policy.authorizes(&append));
    assert!(own_policy.authorizes(&own));
    assert!(!own_policy.authorizes(&all));
    let all_policy = SplicePolicy::Replace {
        scope: ReplaceScope::AllPending,
    };
    assert!(all_policy.authorizes(&all));
}

#[test]
fn delegation_permits_equal_or_lower_authority_only() {
    let cap = SplicePolicy::Replace {
        scope: ReplaceScope::OwnBatches,
    };
    assert!(cap.may_delegate(SplicePolicy::Deny));
    assert!(cap.may_delegate(SplicePolicy::Append));
    assert!(cap.may_delegate(cap), "equal-authority chaining is allowed");
    assert!(!cap.may_delegate(SplicePolicy::Replace {
        scope: ReplaceScope::AllPending
    }));
}

// ── Fragment validation ───────────────────────────────────────────────────

#[test]
fn a_well_formed_fragment_validates() {
    assert_eq!(validate_fragment(&one_node_fragment()), Ok(()));
}

#[test]
fn fragment_validation_reuses_the_shared_invariant_engine() {
    // A dangling edge target is the graph validator's `UnknownTarget`; the
    // fragment path must report it too, not through a second validator.
    let mut fragment = one_node_fragment();
    fragment.nodes[0].routing = Routing::next(Edge::always(EdgeId::new(0), NodeId::new(9)));
    let errors = validate_fragment(&fragment).unwrap_err();
    assert!(
        errors.iter().any(|e| matches!(
            &e.kind,
            FragmentErrorKind::Structure(ValidationError::UnknownTarget { .. })
        )),
        "{errors:?}"
    );
}

#[test]
fn a_nonempty_fragment_needs_entries() {
    let mut fragment = one_node_fragment();
    fragment.entries.clear();
    let errors = validate_fragment(&fragment).unwrap_err();
    assert!(
        errors.iter().any(|e| matches!(
            &e.kind,
            FragmentErrorKind::Structure(ValidationError::NoEntry)
        )),
        "{errors:?}"
    );
}

#[test]
fn exits_must_resolve_and_not_repeat() {
    let mut fragment = one_node_fragment();
    fragment.exits = vec![NodeId::new(3)];
    let errors = validate_fragment(&fragment).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(&e.kind, FragmentErrorKind::UnknownExit(_))),
        "{errors:?}"
    );

    let mut fragment = one_node_fragment();
    fragment.exits = vec![NodeId::new(0), NodeId::new(0)];
    let errors = validate_fragment(&fragment).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(&e.kind, FragmentErrorKind::DuplicateExit(_))),
        "{errors:?}"
    );
}

#[test]
fn a_fragment_is_executable_ir_never_hir() {
    let mut fragment = one_node_fragment();
    fragment.nodes[0].expand = Some(ir::Expansion::ForEach {
        items: ir::ExprId::new(0),
        target: ir::ExpandTarget::Node,
        max_parallel: None,
        fail_fast: false,
    });
    let errors = validate_fragment(&fragment).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(&e.kind, FragmentErrorKind::ExpansionInFragment(_))),
        "{errors:?}"
    );
}

#[test]
fn an_empty_fragment_is_valid_only_under_replace() {
    let empty = GraphFragment::new();
    assert_eq!(validate_fragment(&empty), Ok(()));

    let append = SpliceRequest::append(empty.clone());
    let errors = validate_request(&append).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(&e.kind, FragmentErrorKind::EmptyAppend)),
        "{errors:?}"
    );

    let replace = SpliceRequest::replace(ReplaceScope::OwnBatches, empty);
    assert_eq!(validate_request(&replace), Ok(()));
}

#[test]
fn an_attachment_must_name_a_fragment_node() {
    let request = SpliceRequest::append(one_node_fragment()).with_attachment(
        Attachment::DependsOn {
            node: NodeId::new(7),
            on: ExistingNodeRef::new("build"),
        },
    );
    let errors = validate_request(&request).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(&e.kind, FragmentErrorKind::AttachmentUnknownNode(_))),
        "{errors:?}"
    );
}

// ── Serialized shapes ─────────────────────────────────────────────────────

#[test]
fn spaced_ids_serialize_as_bare_numbers() {
    // The marker is phantom: the wire shape is the raw integer, in both spaces.
    assert_eq!(serde_json::to_value(NodeId::<Local>::new(3)).unwrap(), 3);
    assert_eq!(serde_json::to_value(NodeId::<ir::Live>::new(3)).unwrap(), 3);
    let back: NodeId<Local> = serde_json::from_value(json!(3)).unwrap();
    assert_eq!(back, NodeId::new(3));
}

#[test]
fn splice_policy_and_splices_default_on_deserialization() {
    // A node serialized before v6 has no `splice_policy`; the default is Deny.
    let node: ir::Node = serde_json::from_value(json!({
        "id": 0,
        "name": "n",
        "scope": 0,
        "step": { "kind": "noop", "config": null },
        "join": "All",
        "precondition": null,
        "routing": { "groups": [] },
        "budget": { "max_firings": 1, "timeout": { "secs": 3600, "nanos": 0 } },
        "retry": {
            "max_attempts": 1,
            "backoff": {
                "initial": { "secs": 1, "nanos": 0 },
                "factor": 2.0,
                "max": { "secs": 60, "nanos": 0 },
                "jitter": true
            },
            "retry_on": { "statuses": ["Failure", "TimedOut"], "failure_classes": [] },
            "on_exhaustion": "Fail"
        },
        "expand": null
    }))
    .unwrap();
    assert_eq!(node.splice_policy, SplicePolicy::Deny);

    let outcome: Outcome = serde_json::from_value(json!({
        "status": "Success",
        "output": null,
        "metrics": {}
    }))
    .unwrap();
    assert!(outcome.splices.is_empty());
    // An empty list stays off the wire, so non-splice outcomes keep their shape.
    let round = serde_json::to_value(&outcome).unwrap();
    assert!(round.get("splices").is_none());
}

#[test]
fn a_request_round_trips_through_serde() {
    let request = SpliceRequest::append(one_node_fragment()).with_attachment(
        Attachment::DependsOn {
            node: NodeId::new(0),
            on: ExistingNodeRef::new("build#2"),
        },
    );
    let json = serde_json::to_value(&request).unwrap();
    let back: SpliceRequest = serde_json::from_value(json).unwrap();
    assert_eq!(back, request);
}
