//! §3: expressions are evaluated in the pure core. Total, deterministic, no IO.

use ir::expr::{EvalError, eval, eval_bool};
use ir::{BinOp, EvalEnv, ExprTable, RunContext, StaticCtx, UnOp, truthy};
use serde_json::{Value, json};

/// A static-only environment: no token, no run context.
struct Env {
    statics: StaticCtx,
    run: RunContext,
    token: Value,
}

impl Env {
    fn env(&self) -> EvalEnv<'_> {
        EvalEnv::new(&self.token, &self.run, &self.statics)
    }
}

fn ctx(pairs: &[(&str, Value)]) -> Env {
    let mut statics = StaticCtx::new();
    for (k, v) in pairs {
        statics.set(k, v.clone());
    }
    Env {
        statics,
        run: RunContext::new(),
        token: Value::Null,
    }
}

fn empty() -> Env {
    ctx(&[])
}

impl Env {
    /// The token payload, which `token` and `input` resolve to.
    fn with_token(mut self, token: Value) -> Self {
        self.token = token;
        self
    }

    fn with_node(mut self, name: &str, record: ir::NodeRecord) -> Self {
        self.run.record(smol_str::SmolStr::new(name), record);
        self
    }

    fn with_kv(mut self, key: &str, value: Value) -> Self {
        self.run.kv.insert(smol_str::SmolStr::new(key), value);
        self
    }
}

#[test]
fn literals_and_variables() {
    let mut t = ExprTable::new();
    let lit = t.lit(7);
    let var = t.var("output");
    let c = ctx(&[("output", json!("hello"))]);
    assert_eq!(eval(&t, lit, &c.env()).unwrap(), json!(7));
    assert_eq!(eval(&t, var, &c.env()).unwrap(), json!("hello"));
    assert_eq!(
        eval(&t, var, &empty().env()).unwrap_err(),
        EvalError::UnboundVar("output".into())
    );
}

#[test]
fn field_and_index_access() {
    let mut t = ExprTable::new();
    let region = t.path("input", &["config", "region"]);
    let items = t.var("items");
    let idx = t.lit(1);
    let second = t.index(items, idx);
    let c =
        ctx(&[("items", json!(["a", "b", "c"]))]).with_token(json!({"config": {"region": "eu"}}));
    assert_eq!(eval(&t, region, &c.env()).unwrap(), json!("eu"));
    assert_eq!(eval(&t, second, &c.env()).unwrap(), json!("b"));

    // A missing field is null, not an error: guards stay total.
    let missing = t.path("input", &["nope", "deeper"]);
    assert_eq!(eval(&t, missing, &c.env()).unwrap(), Value::Null);
}

#[test]
fn arithmetic_keeps_integers_integral() {
    let mut t = ExprTable::new();
    let idx = t.var("idx");
    let one = t.lit(1);
    let next = t.binary(BinOp::Add, idx, one);
    let c = ctx(&[("idx", json!(2))]);
    assert_eq!(eval(&t, next, &c.env()).unwrap(), json!(3));
    assert!(eval(&t, next, &c.env()).unwrap().is_i64(), "3, not 3.0");
}

#[test]
fn comparison_and_boolean_operators() {
    let mut t = ExprTable::new();
    let a = t.var("a");
    let b = t.var("b");
    let lt = t.binary(BinOp::Lt, a, b);
    let eq = t.binary(BinOp::Eq, a, b);
    let and = t.binary(BinOp::And, lt, eq);
    let or = t.binary(BinOp::Or, lt, eq);
    let not = t.unary(UnOp::Not, lt);
    let c = ctx(&[("a", json!(1)), ("b", json!(2))]);
    assert_eq!(eval(&t, lt, &c.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, eq, &c.env()).unwrap(), json!(false));
    assert_eq!(eval(&t, and, &c.env()).unwrap(), json!(false));
    assert_eq!(eval(&t, or, &c.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, not, &c.env()).unwrap(), json!(false));
}

/// `and` and `or` short-circuit, so a bad right side is never reached.
#[test]
fn boolean_operators_short_circuit() {
    let mut t = ExprTable::new();
    let no = t.lit(false);
    let unbound = t.var("never_bound");
    let and = t.binary(BinOp::And, no, unbound);
    assert_eq!(eval(&t, and, &empty().env()).unwrap(), json!(false));

    let yes = t.lit(true);
    let or = t.binary(BinOp::Or, yes, unbound);
    assert_eq!(eval(&t, or, &empty().env()).unwrap(), json!(true));
}

#[test]
fn concatenation_covers_arrays_strings_and_objects() {
    let mut t = ExprTable::new();
    let acc = t.var("acc");
    let output = t.var("output");
    let one = t.array(vec![output]);
    let next = t.binary(BinOp::Concat, acc, one);
    let c = ctx(&[("acc", json!([1, 2])), ("output", json!(3))]);
    assert_eq!(eval(&t, next, &c.env()).unwrap(), json!([1, 2, 3]));

    let left = t.lit("a");
    let right = t.lit("b");
    let joined = t.binary(BinOp::Concat, left, right);
    assert_eq!(eval(&t, joined, &empty().env()).unwrap(), json!("ab"));

    let l = t.lit(json!({"x": 1}));
    let r = t.lit(json!({"y": 2}));
    let merged = t.binary(BinOp::Concat, l, r);
    assert_eq!(
        eval(&t, merged, &empty().env()).unwrap(),
        json!({"x": 1, "y": 2})
    );
}

#[test]
fn status_functions_read_the_bound_status() {
    let mut t = ExprTable::new();
    let always = t.call("always", vec![]);
    let success = t.call("success", vec![]);
    let failure = t.call("failure", vec![]);
    let skipped = t.call("skipped", vec![]);

    let c = ctx(&[("status", json!("failure"))]);
    assert_eq!(eval(&t, always, &c.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, success, &c.env()).unwrap(), json!(false));
    assert_eq!(eval(&t, failure, &c.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, skipped, &c.env()).unwrap(), json!(false));

    // With no status bound at all, `always()` still holds and the rest are false.
    assert_eq!(eval(&t, always, &empty().env()).unwrap(), json!(true));
    assert_eq!(eval(&t, failure, &empty().env()).unwrap(), json!(false));
}

#[test]
fn list_functions_order_and_pluck() {
    let mut t = ExprTable::new();
    let inputs = t.var("inputs");
    let index_key = t.lit("index");
    let sorted = t.call("sort_by_key", vec![inputs, index_key]);
    let value_key = t.lit("value");
    let ordered = t.call("pluck", vec![sorted, value_key]);
    let c = ctx(&[(
        "inputs",
        json!([
            {"index": 2, "value": "c"},
            {"index": 0, "value": "a"},
            {"index": 1, "value": "b"},
        ]),
    )]);
    assert_eq!(eval(&t, ordered, &c.env()).unwrap(), json!(["a", "b", "c"]));
}

#[test]
fn helper_functions() {
    let mut t = ExprTable::new();
    let items = t.var("items");
    let length = t.call("len", vec![items]);
    let needle = t.lit("b");
    let has = t.call("contains", vec![items, needle]);
    let null = t.lit(Value::Null);
    let fallback = t.lit("fallback");
    let defaulted = t.call("default", vec![null, fallback]);
    let c = ctx(&[("items", json!(["a", "b"]))]);
    assert_eq!(eval(&t, length, &c.env()).unwrap(), json!(2));
    assert_eq!(eval(&t, has, &c.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, defaulted, &c.env()).unwrap(), json!("fallback"));
}

#[test]
fn objects_arrays_and_conditionals() {
    let mut t = ExprTable::new();
    let idx = t.var("idx");
    let one = t.lit(1);
    let next = t.binary(BinOp::Add, idx, one);
    let acc = t.var("acc");
    let state = t.object(vec![("idx", next), ("acc", acc)]);
    let c = ctx(&[("idx", json!(0)), ("acc", json!([]))]);
    assert_eq!(
        eval(&t, state, &c.env()).unwrap(),
        json!({"idx": 1, "acc": []})
    );

    let yes = t.lit("yes");
    let no = t.lit("no");
    let cond = t.cond(idx, yes, no);
    assert_eq!(eval(&t, cond, &c.env()).unwrap(), json!("no"), "0 is falsy");
}

#[test]
fn errors_are_values_not_panics() {
    let mut t = ExprTable::new();
    let text = t.lit("abc");
    let number = t.lit(1);
    let bad = t.binary(BinOp::Sub, text, number);
    assert!(matches!(
        eval(&t, bad, &empty().env()),
        Err(EvalError::Type { .. })
    ));

    let zero = t.lit(0);
    let div = t.binary(BinOp::Div, number, zero);
    assert_eq!(eval(&t, div, &empty().env()), Err(EvalError::DivByZero));

    let unknown = t.call("nope", vec![]);
    assert!(matches!(
        eval(&t, unknown, &empty().env()),
        Err(EvalError::UnknownFunction(_))
    ));

    let wrong_arity = t.call("len", vec![]);
    assert!(matches!(
        eval(&t, wrong_arity, &empty().env()),
        Err(EvalError::Arity { .. })
    ));
}

#[test]
fn truthiness_matches_the_documented_rules() {
    assert!(!truthy(&Value::Null));
    assert!(!truthy(&json!(false)));
    assert!(!truthy(&json!(0)));
    assert!(!truthy(&json!("")));
    assert!(!truthy(&json!([])));
    assert!(!truthy(&json!({})));
    assert!(truthy(&json!(1)));
    assert!(truthy(&json!("x")));
    assert!(truthy(&json!([0])));
}

#[test]
fn eval_bool_uses_truthiness() {
    let mut t = ExprTable::new();
    let items = t.var("items");
    let c = ctx(&[("items", json!(["a"]))]);
    assert!(eval_bool(&t, items, &c.env()).unwrap());
}

/// `token` and `input` come from the environment, not from the statics, and they
/// shadow a static of the same name.
#[test]
fn the_token_shadows_the_statics() {
    let mut t = ExprTable::new();
    let token = t.var("token");
    let input = t.var("input");
    let c = ctx(&[("input", json!("from statics"))]).with_token(json!("from token"));
    assert_eq!(eval(&t, token, &c.env()).unwrap(), json!("from token"));
    assert_eq!(eval(&t, input, &c.env()).unwrap(), json!("from token"));
}

/// `nodes.*` reads the run context: one way for an expression to see upstream state.
#[test]
fn nodes_reads_the_run_context() {
    let mut t = ExprTable::new();
    let status = t.path("nodes", &["build", "status"]);
    let output = t.path("nodes", &["build", "output"]);
    let attempts = t.path("nodes", &["build", "attempts"]);
    let missing = t.path("nodes", &["nope", "status"]);

    let c = empty().with_node(
        "build",
        ir::NodeRecord {
            status: ir::Status::Success,
            output: json!({"artifact": "app.tar"}),
            generation: ir::Generation::ZERO,
            attempts: 3,
        },
    );
    assert_eq!(eval(&t, status, &c.env()).unwrap(), json!("success"));
    assert_eq!(
        eval(&t, output, &c.env()).unwrap(),
        json!({"artifact": "app.tar"})
    );
    assert_eq!(eval(&t, attempts, &c.env()).unwrap(), json!(3));
    assert_eq!(eval(&t, missing, &c.env()).unwrap(), Value::Null);
}

/// `kv.*` reads run-scoped key/value state.
#[test]
fn kv_reads_the_run_context() {
    let mut t = ExprTable::new();
    let value = t.path("kv", &["deploy_target"]);
    let c = empty().with_kv("deploy_target", json!("staging"));
    assert_eq!(eval(&t, value, &c.env()).unwrap(), json!("staging"));
}

/// `success()` is success-like, so it covers `PartialSuccess`. A guard that needs to
/// tell them apart calls `partial_success()` or `full_success()`.
#[test]
fn success_is_success_like() {
    let mut t = ExprTable::new();
    let success = t.call("success", vec![]);
    let partial = t.call("partial_success", vec![]);
    let full = t.call("full_success", vec![]);
    let failure = t.call("failure", vec![]);

    let clean = ctx(&[("status", json!("success"))]);
    assert_eq!(eval(&t, success, &clean.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, partial, &clean.env()).unwrap(), json!(false));
    assert_eq!(eval(&t, full, &clean.env()).unwrap(), json!(true));

    let soft = ctx(&[("status", json!("partial_success"))]);
    assert_eq!(
        eval(&t, success, &soft.env()).unwrap(),
        json!(true),
        "a soft failure still satisfies the default success guard"
    );
    assert_eq!(eval(&t, partial, &soft.env()).unwrap(), json!(true));
    assert_eq!(eval(&t, full, &soft.env()).unwrap(), json!(false));
    assert_eq!(eval(&t, failure, &soft.env()).unwrap(), json!(false));
}
