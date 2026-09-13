//! §3/§5: a scope is a resource scope, not a sequence. The host acquires it
//! before the first step in it runs and releases it once nothing in it can run
//! again.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use engine::Command;
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    ExprOrValue, GraphBuilder, JoinPolicy, Outcome, RunStatus, RuntimeSpec, Scope, ScopeId,
    StepRef, Value, WorkspacePolicy, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

fn scope_events(h: &Harness) -> Vec<(String, u32)> {
    h.commands
        .iter()
        .filter_map(|c| match c {
            Command::AcquireScope { scope } => Some(("acquire".to_string(), scope.raw())),
            Command::ReleaseScope { scope } => Some(("release".to_string(), scope.raw())),
            _ => None,
        })
        .collect()
}

/// A chain of steps in one scope acquires it once and holds it to the end.
#[test]
fn a_scope_is_held_across_its_whole_chain() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let step_a = b.add_step("a", scope, NOOP);
    let step_c = b.add_step("c", scope, NOOP);
    let step_d = b.add_step("d", scope, NOOP);
    b.link(step_a, step_c);
    b.link(step_c, step_d);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(
        scope_events(&h),
        vec![("acquire".to_string(), 0), ("release".to_string(), 0)],
        "one acquire, one release, not one pair per step"
    );
}

/// Two jobs are two scopes, each acquired and released on its own.
#[test]
fn independent_jobs_get_independent_scopes() {
    let mut b = GraphBuilder::bare();
    let build = b.add_scope(Scope::new(ScopeId::new(0)));
    let deploy = b.add_scope(
        Scope::new(ScopeId::new(0))
            .with_runtime(RuntimeSpec::container("deployer:1").requiring(&["linux", "amd64"]))
            .with_workspace(WorkspacePolicy::PerNode),
    );
    let compile = b.add_step("compile", build, NOOP);
    let ship = b.add_step("ship", deploy, NOOP);
    b.link(compile, ship);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    let events = scope_events(&h);
    assert_eq!(events[0], ("acquire".to_string(), 0));
    assert!(events.contains(&("acquire".to_string(), 1)));
    assert!(events.contains(&("release".to_string(), 0)));
    assert!(events.contains(&("release".to_string(), 1)));
}

/// Scope env is visible to expressions, and to a step's resolved config. An env
/// value may itself be an expression, resolved at firing time.
#[test]
fn scope_env_reaches_the_step_config() {
    let mut b = GraphBuilder::bare();
    let computed = b.exprs().lit(42);
    let scope = b.add_scope(
        Scope::new(ScopeId::new(0))
            .with_env("REGION", ExprOrValue::Value(json!("us-east")))
            .with_env("ANSWER", ExprOrValue::Expr(computed)),
    );
    let node = b.add_step("deploy", scope, NOOP);
    let env = b.exprs().var("env");
    b.node_mut(node).step =
        StepRef::new(NOOP, json!({ "env": { EXPR_PLACEHOLDER_KEY: env.raw() } }));
    let graph = b.build();
    validate(&graph).expect("valid");

    let seen = Rc::new(RefCell::new(Value::Null));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| {
        *sink.borrow_mut() = info.config.clone();
        Outcome::success(Value::Null)
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(
        seen.borrow()["env"],
        json!({ "REGION": "us-east", "ANSWER": 42 })
    );
}

/// A scope already in use stays held while a token waits on a join inside it,
/// even though nothing in that scope is running at that moment.
#[test]
fn a_scope_stays_held_while_a_join_waits() {
    let mut b = GraphBuilder::bare();
    let main = b.add_scope(Scope::new(ScopeId::new(0)));
    let other = b.add_scope(Scope::new(ScopeId::new(0)));
    let start = b.add_step("start", main, NOOP);
    let fast = b.add_step("fast", main, NOOP);
    let slow = b.add_step("slow", other, NOOP);
    let join = b.add_step("join", main, NOOP);
    b.fan_out(start, &[fast, slow]);
    b.link(fast, join);
    b.link(slow, join);
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(engine::Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(Value::Null));
    let branches = h.take_starts();
    assert_eq!(branches.len(), 2);

    // Finish the branch in `main`. Nothing in `main` is running now, but its token
    // is waiting at the join, so the scope must stay up.
    let fast_firing = branches
        .iter()
        .find(|(_, name)| name == "fast")
        .expect("fast started")
        .0;
    h.finish(fast_firing, Outcome::success(Value::Null));
    assert!(
        h.state.held_scopes().any(|s| s == main),
        "the waiting token keeps its scope alive"
    );

    let slow_firing = branches
        .iter()
        .find(|(_, name)| name == "slow")
        .expect("slow started")
        .0;
    h.finish(slow_firing, Outcome::success(Value::Null));
    let join_start = h.take_starts();
    assert_eq!(join_start.len(), 1);
    h.finish(join_start[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Success));
    assert_eq!(h.state.held_scopes().count(), 0, "everything is released");
}

/// A scope-env expression that cannot evaluate fails the firing with the cause
/// on the record — class `firing_env`, message naming the error — so a report
/// over the log can say *which* expression broke, not only that one did.
#[test]
fn a_broken_scope_env_names_its_cause_on_the_record() {
    let mut b = GraphBuilder::bare();
    let bad = {
        let t = b.exprs();
        let arg = t.lit("{broken");
        t.call("from_json", vec![arg])
    };
    let scope =
        b.add_scope(Scope::new(ScopeId::new(0)).with_env("VERSION", ExprOrValue::Expr(bad)));
    b.add_step("only", scope, NOOP);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Failed);
    let record = &h.state.history()[0];
    let info = record.outcome.status.failure_info().expect("a failure");
    assert_eq!(info.class, engine::FIRING_ENV_CLASS);
    assert!(
        info.message
            .strip_prefix("could not build the firing environment: ")
            .is_some_and(|cause| !cause.is_empty()),
        "the cause rides the message: {}",
        info.message
    );
    assert!(
        !h.state.errors().is_empty(),
        "the error list carries it too"
    );
}
