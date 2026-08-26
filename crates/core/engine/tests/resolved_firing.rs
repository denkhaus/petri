//! `ResolvedFiring` is where "no unresolved `ExprId` crosses the executor boundary"
//! is enforced. The invariant lives in the type, not in a review comment.

mod support;

use engine::{Command, EngineState, Event, ResolvedFiring, RunError, apply};
use ir::placeholder::{EXPR_PLACEHOLDER_KEY, SECRET_REF_KEY};
use ir::{
    Attempt, FiringId, Generation, GraphBuilder, NodeId, RunStatus, ScopeId, StepRef, Value,
    validate,
};
use serde_json::json;
use support::{Harness, NOOP};

fn firing(config: Value) -> Result<ResolvedFiring, engine::UnresolvedConfig> {
    ResolvedFiring::new(
        FiringId::new(1),
        NodeId::new(0),
        Generation::ZERO,
        Attempt::FIRST,
        ScopeId::new(0),
        vec![],
        config,
    )
}

/// A clean config builds; one holding a placeholder does not.
#[test]
fn the_constructor_refuses_an_unresolved_config() {
    let ok = firing(json!({ "region": "us-east" })).expect("resolved config is accepted");
    assert_eq!(ok.config(), &json!({ "region": "us-east" }));

    let err = firing(json!({ "env": { "TOKEN": { EXPR_PLACEHOLDER_KEY: 3 } } }))
        .expect_err("a placeholder must be refused");
    assert_eq!(err.node, NodeId::new(0));
    assert_eq!(err.path, "env.TOKEN");
    assert_eq!(err.reason, engine::BoundaryViolation::UnresolvedExpression);
}

/// A secret reference is the one non-literal form that may cross the boundary. It
/// has to: a `ResolvedFiring` is serialized into the event log, so resolving the
/// value here would write the secret to disk.
#[test]
fn a_secret_reference_is_allowed_across_the_boundary() {
    let ok = firing(json!({ "env": { "TOKEN": { SECRET_REF_KEY: "DEPLOY_KEY" } } }))
        .expect("a secret reference is permitted");
    assert_eq!(
        ok.config()["env"]["TOKEN"][SECRET_REF_KEY],
        json!("DEPLOY_KEY"),
        "the reference crosses intact; the value is fetched at spawn"
    );

    // The name must be a string. Anything else is refused here rather than reaching
    // a step as literal JSON.
    let err = firing(json!({ "env": { "TOKEN": { SECRET_REF_KEY: ["not", "a", "name"] } } }))
        .expect_err("a malformed reference must be refused");
    assert_eq!(err.path, "env.TOKEN");
    assert_eq!(err.reason, engine::BoundaryViolation::MalformedSecretRef);
}

/// The sibling of the expression wire-tamper test: hand-editing a secret reference
/// into a bad shape on the wire is caught on the way back in.
#[test]
fn deserialization_rejects_a_tampered_secret_reference() {
    let clean = firing(json!({ "env": { "TOKEN": { SECRET_REF_KEY: "DEPLOY_KEY" } } })).unwrap();
    let encoded = serde_json::to_string(&clean).expect("encode");
    let decoded: ResolvedFiring = serde_json::from_str(&encoded).expect("a good one decodes");
    assert_eq!(decoded, clean);

    let tampered = encoded.replace(r#"{"$secret":"DEPLOY_KEY"}"#, r#"{"$secret":{"nested":1}}"#);
    assert!(
        serde_json::from_str::<ResolvedFiring>(&tampered).is_err(),
        "a malformed secret reference must not deserialize"
    );

    // And the secret's *value* is nowhere in the encoded form, only its name.
    assert!(encoded.contains("DEPLOY_KEY"));
    assert!(!encoded.contains("s3cr3t-value"));
}

/// Deserialization goes through the same constructor, so a value read off the wire
/// carries the invariant too.
#[test]
fn deserialization_cannot_smuggle_a_placeholder_through() {
    let clean = firing(json!({ "region": "eu" })).unwrap();
    let encoded = serde_json::to_string(&clean).expect("encode");
    let decoded: ResolvedFiring = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, clean);

    // The same wire shape, with a placeholder put back into the config by hand.
    let smuggled = encoded.replace(r#"{"region":"eu"}"#, r#"{"region":{"$expr":3}}"#);
    assert!(
        serde_json::from_str::<ResolvedFiring>(&smuggled).is_err(),
        "deserializing an unresolved config must fail"
    );
}

/// The core resolves config against the firing context before handing it over.
#[test]
fn the_core_resolves_config_before_the_boundary() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    let input = b.exprs().var("input");
    b.node_mut(c).step = StepRef::new(
        NOOP,
        json!({ "from_upstream": { EXPR_PLACEHOLDER_KEY: input.raw() } }),
    );
    let graph = b.build();
    validate(&graph).expect("valid as HIR");

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "a" {
            ir::Outcome::success(json!("carried"))
        } else {
            assert_eq!(info.config, json!({ "from_upstream": "carried" }));
            ir::Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Success);

    // Every command the run produced carries a placeholder-free config.
    for command in &h.commands {
        if let Command::StartStep(resolved) = command {
            assert!(!ir::placeholder::contains_placeholder(resolved.config()));
        }
    }
}

/// A placeholder the resolver cannot read — `$expr` with a non-numeric value — is
/// caught at the boundary and fails that node, instead of reaching a step.
#[test]
fn a_malformed_placeholder_fails_the_node_at_the_boundary() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    b.node_mut(a).step = StepRef::new(
        NOOP,
        json!({ "broken": { EXPR_PLACEHOLDER_KEY: "not an expression id" } }),
    );
    let graph = b.build();

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("a"), 0, "the step never ran");
    assert!(matches!(
        h.state.errors().first(),
        Some(RunError::UnresolvedConfig { path, .. }) if path == "broken"
    ));
}

/// `validate_plan` catches the same thing at load time, so the runtime check is a
/// backstop rather than the only line of defence.
#[test]
fn load_time_validation_catches_it_first() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let value = b.exprs().lit(1);
    b.node_mut(a).step = StepRef::new(NOOP, json!({ "x": { EXPR_PLACEHOLDER_KEY: value.raw() } }));
    let graph = b.build();

    let errors = ir::validate_plan(&graph).expect_err("not an executable plan");
    assert!(errors.iter().any(|e| matches!(
        e,
        ir::ValidationError::HirConfigInPlan { path, .. } if path == "x"
    )));
}

/// The payload names the firing, so a host can correlate it with the events it
/// sends back.
#[test]
fn the_payload_identifies_the_firing() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_step("only", scope, NOOP);
    let graph = b.build();

    let (state, commands) = apply(EngineState::new(graph), Event::RunStarted);
    let Some(Command::StartStep(resolved)) =
        commands.iter().find(|c| matches!(c, Command::StartStep(_)))
    else {
        panic!("expected a StartStep");
    };
    assert_eq!(resolved.node(), NodeId::new(0));
    assert_eq!(resolved.generation(), Generation::ZERO);
    assert_eq!(resolved.attempt(), Attempt::FIRST);
    assert_eq!(resolved.scope(), ScopeId::new(0));
    assert_eq!(resolved.inputs().len(), 1, "the seed token");
    assert!(state.firing(resolved.id()).is_some());
}
