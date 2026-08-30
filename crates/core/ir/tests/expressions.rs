//! §3: expressions are evaluated in the pure core. Total, deterministic, no IO.

use std::slice;

use ir::expr::{EvalError, eval, eval_bool};
use ir::{BinOp, EvalEnv, ExprTable, RunContext, StaticCtx, UnOp, truthy};
use serde_json::{Value, json};

/// A static-only environment: no token, no run context.
struct Env {
    statics: StaticCtx,
    run:     RunContext,
    token:   Value,
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

/// A completed `build` node: a success with one output field.
fn build_record(attempts: u32) -> ir::NodeRecord {
    ir::NodeRecord {
        status: ir::Status::Success,
        output: json!({"artifact": "app.tar"}),
        generation: ir::Generation::ZERO,
        attempts,
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

/// `token` and `input` come from the environment, not from the statics, and
/// they shadow a static of the same name.
#[test]
fn the_token_shadows_the_statics() {
    let mut t = ExprTable::new();
    let token = t.var("token");
    let input = t.var("input");
    let c = ctx(&[("input", json!("from statics"))]).with_token(json!("from token"));
    assert_eq!(eval(&t, token, &c.env()).unwrap(), json!("from token"));
    assert_eq!(eval(&t, input, &c.env()).unwrap(), json!("from token"));
}

/// `nodes.*` reads the run context: one way for an expression to see upstream
/// state.
#[test]
fn nodes_reads_the_run_context() {
    let mut t = ExprTable::new();
    let status = t.path("nodes", &["build", "status"]);
    let output = t.path("nodes", &["build", "output"]);
    let attempts = t.path("nodes", &["build", "attempts"]);
    let missing = t.path("nodes", &["nope", "status"]);

    let c = empty().with_node("build", build_record(3));
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

/// `nodes.x`, `nodes["x"]`, `kv.x` and `kv["x"]` read a single entry without
/// materializing the whole map. The result must be exactly what indexing the
/// whole map would give: for an entry that is there, one that is not, and an
/// index that is not a string.
#[test]
fn single_entry_reads_match_the_whole_map() {
    let mut t = ExprTable::new();
    let all_nodes = t.var("nodes");
    let one_node = t.path("nodes", &["build"]);
    let no_node = t.path("nodes", &["nope"]);
    let build = t.lit("build");
    let one_node_indexed = t.index(all_nodes, build);
    let zero = t.lit(0);
    let non_string_index = t.index(all_nodes, zero);
    let all_kv = t.var("kv");
    let one_kv = t.path("kv", &["deploy_target"]);
    let no_kv = t.path("kv", &["nope"]);
    let deploy_target = t.lit("deploy_target");
    let one_kv_indexed = t.index(all_kv, deploy_target);

    let c = empty()
        .with_node("build", build_record(1))
        .with_kv("deploy_target", json!("staging"));

    let nodes = eval(&t, all_nodes, &c.env()).unwrap();
    assert_eq!(eval(&t, one_node, &c.env()).unwrap(), nodes["build"]);
    assert_eq!(
        eval(&t, one_node_indexed, &c.env()).unwrap(),
        nodes["build"]
    );
    assert_eq!(eval(&t, no_node, &c.env()).unwrap(), nodes["nope"]);
    assert_eq!(eval(&t, non_string_index, &c.env()).unwrap(), Value::Null);

    let kv = eval(&t, all_kv, &c.env()).unwrap();
    assert_eq!(eval(&t, one_kv, &c.env()).unwrap(), kv["deploy_target"]);
    assert_eq!(
        eval(&t, one_kv_indexed, &c.env()).unwrap(),
        kv["deploy_target"]
    );
    assert_eq!(eval(&t, no_kv, &c.env()).unwrap(), kv["nope"]);
}

/// `success()` is success-like, so it covers `PartialSuccess`. A guard that
/// needs to tell them apart calls `partial_success()` or `full_success()`.
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

/// `split` turns a step's string output into a list, which is what feeds
/// `for_each`.
#[test]
fn split_turns_output_into_a_list() {
    let mut t = ExprTable::new();
    let text = t.var("regions");
    let newline = t.lit("\n");
    let parts = t.call("split", vec![text, newline]);

    let c = ctx(&[("regions", json!("us-east\nus-west\neu\n"))]);
    assert_eq!(
        eval(&t, parts, &c.env()).unwrap(),
        json!(["us-east", "us-west", "eu"]),
        "a trailing newline does not produce an empty entry"
    );

    let comma = t.lit(",");
    let by_comma = t.call("split", vec![text, comma]);
    let c = ctx(&[("regions", json!("a,b"))]);
    assert_eq!(eval(&t, by_comma, &c.env()).unwrap(), json!(["a", "b"]));

    // An empty separator has no sensible meaning, and is an error rather than a
    // silent character split.
    let empty = t.lit("");
    let bad = t.call("split", vec![text, empty]);
    assert!(matches!(
        eval(&t, bad, &c.env()),
        Err(EvalError::Type { .. })
    ));
}

/// `matches`: regex search in a string — the one builtin a frontend condition
/// grammar with a regex operator (`context.version matches ^v\d+`) forces.
#[test]
fn matches_searches_a_string() {
    let mut t = ExprTable::new();
    let text = t.var("text");
    let version = t.lit(r"^v\d+");
    let versioned = t.call("matches", vec![text, version]);

    let c = ctx(&[("text", json!("v12.3"))]);
    assert_eq!(eval(&t, versioned, &c.env()).unwrap(), json!(true));
    let c = ctx(&[("text", json!("release-v12"))]);
    assert_eq!(
        eval(&t, versioned, &c.env()).unwrap(),
        json!(false),
        "the pattern's own anchor holds"
    );

    // Unanchored by default: a bare pattern searches anywhere in the string.
    let bare = t.lit(r"v\d+");
    let anywhere = t.call("matches", vec![text, bare]);
    assert_eq!(eval(&t, anywhere, &c.env()).unwrap(), json!(true));

    // Character classes.
    let sha = t.lit(r"^[0-9a-f]{7}$");
    let is_sha = t.call("matches", vec![text, sha]);
    let c = ctx(&[("text", json!("902a62e"))]);
    assert_eq!(eval(&t, is_sha, &c.env()).unwrap(), json!(true));
    let c = ctx(&[("text", json!("not-a-sha"))]);
    assert_eq!(eval(&t, is_sha, &c.env()).unwrap(), json!(false));
}

/// `matches` is total: bad inputs are typed errors, never panics.
#[test]
fn matches_rejects_bad_inputs_with_typed_errors() {
    let mut t = ExprTable::new();

    // Non-string text is a type error, consistent with `contains`. Coercion is the
    // lowering's job (see the equivalence test below).
    let number = t.lit(7);
    let any = t.lit(".*");
    let non_string = t.call("matches", vec![number, any]);
    assert!(matches!(
        eval(&t, non_string, &empty().env()),
        Err(EvalError::Type { .. })
    ));

    // An invalid pattern is a typed error, never a panic. Frontends validate
    // patterns at parse time, so runtime never sees one; totality holds anyway.
    let text = t.lit("abc");
    let unclosed = t.lit("(unclosed");
    let invalid = t.call("matches", vec![text, unclosed]);
    assert!(matches!(
        eval(&t, invalid, &empty().env()),
        Err(EvalError::Type { .. })
    ));

    let numeric_pattern = t.lit(1);
    let bad_pattern = t.call("matches", vec![text, numeric_pattern]);
    assert!(matches!(
        eval(&t, bad_pattern, &empty().env()),
        Err(EvalError::Type { .. })
    ));

    let one_arg = t.call("matches", vec![text]);
    assert!(matches!(
        eval(&t, one_arg, &empty().env()),
        Err(EvalError::Arity { .. })
    ));
}

/// The fabro condition lowering is `matches(to_string(default(v, "")),
/// pattern)`: `default` supplies fabro's null-and-missing -> `""` rule, and
/// `to_string` renders the rest exactly as fabro's `json_value_to_string` does
/// — booleans and numbers as text, arrays and objects as JSON. Pinned per value
/// kind with fully anchored patterns, so a drift in any rendering fails here.
#[test]
fn matches_lowering_coerces_like_fabro() {
    let mut t = ExprTable::new();

    let cases: &[(Value, &str, bool)] = &[
        (Value::Null, "^$", true), // null renders as ""
        (Value::Null, "x", false),
        (json!(true), "^true$", true), // booleans as text
        (json!(false), "^false$", true),
        (json!(42), "^42$", true), // numbers as text
        (json!(4.5), r"^4\.5$", true),
        (json!(["a", "b"]), r#"^\["a","b"\]$"#, true), // arrays as JSON
        (json!({"k": 1}), r#"^\{"k":1\}$"#, true),     // objects as JSON
    ];
    for (bound, pattern, want) in cases {
        let value = t.var("value");
        let empty_string = t.lit("");
        let defaulted = t.call("default", vec![value, empty_string]);
        let text = t.call("to_string", vec![defaulted]);
        let pat = t.lit(*pattern);
        let call = t.call("matches", vec![text, pat]);
        let c = ctx(&[("value", bound.clone())]);
        assert_eq!(
            eval(&t, call, &c.env()).unwrap(),
            json!(*want),
            "{bound} against {pattern}"
        );
    }

    // The missing case: an absent field evaluates to null, and the lowering renders
    // it as "" like fabro renders a missing context path.
    let missing = t.path("input", &["version"]);
    let empty_string = t.lit("");
    let defaulted = t.call("default", vec![missing, empty_string]);
    let text = t.call("to_string", vec![defaulted]);
    let empty_pattern = t.lit("^$");
    let call = t.call("matches", vec![text, empty_pattern]);
    let c = empty().with_token(json!({}));
    assert_eq!(eval(&t, call, &c.env()).unwrap(), json!(true));
}

/// The function table gates dispatch, so it cannot drift from the
/// implementation: every entry must evaluate, and a name that is not an entry
/// must be unknown.
#[test]
fn the_builtin_table_matches_the_implementation() {
    use ir::expr::{BUILTINS, builtin};

    let mut t = ExprTable::new();
    let c = ctx(&[("status", json!("success"))]);

    for spec in BUILTINS {
        // Call it with its declared arity. Arguments are deliberately the wrong
        // *types* for most functions: a type error proves the arm was reached, which
        // is what this test is checking.
        let args: Vec<_> = (0..spec.arity).map(|_| t.lit(json!("x"))).collect();
        let call = t.call(spec.name, args);
        match eval(&t, call, &c.env()) {
            Err(EvalError::UnknownFunction(name)) => {
                panic!("`{name}` is in the table but has no implementation")
            }
            Err(EvalError::Arity { name, .. }) => {
                panic!("`{name}` declares the wrong arity in the table")
            }
            _ => {}
        }
    }

    // And nothing outside the table dispatches, however plausible it sounds.
    for name in ["map", "filter", "reduce", "now", "random", "env"] {
        let call = t.call(name, vec![]);
        assert!(
            matches!(
                eval(&t, call, &c.env()),
                Err(EvalError::UnknownFunction(_) | EvalError::Arity { .. })
            ),
            "`{name}` dispatched without being in the table"
        );
    }

    assert!(builtin("split").is_some());
    assert!(builtin("definitely_not_a_builtin").is_none());
}

/// Arity is checked once, from the table, for every function.
#[test]
fn arity_comes_from_the_table() {
    use ir::expr::BUILTINS;

    let mut t = ExprTable::new();
    let c = empty();
    for spec in BUILTINS {
        let too_many: Vec<_> = (0..=spec.arity).map(|_| t.lit(1)).collect();
        let call = t.call(spec.name, too_many);
        assert!(
            matches!(eval(&t, call, &c.env()), Err(EvalError::Arity { .. })),
            "`{}` accepted {} arguments",
            spec.name,
            spec.arity + 1
        );
    }
}

// ── Loose semantics ───────────────────────────────────────────────────────

fn call(t: &mut ExprTable, name: &str, args: &[Value]) -> ir::ExprId {
    let ids: Vec<_> = args.iter().map(|a| t.lit(a.clone())).collect();
    t.call(name, ids)
}

fn run(name: &str, args: &[Value]) -> Value {
    let mut t = ExprTable::new();
    let id = call(&mut t, name, args);
    eval(&t, id, &empty().env()).unwrap_or_else(|e| panic!("{name}{args:?}: {e}"))
}

/// The loose number coercion table.
#[test]
fn loose_number_coercion_table() {
    let cases: &[(Value, Value)] = &[
        (json!(null), json!(0)),
        (json!(true), json!(1)),
        (json!(false), json!(0)),
        (json!(""), json!(0)),
        (json!("  "), json!(0)),
        (json!("42"), json!(42)),
        (json!(" 42 "), json!(42)),
        (json!("-2.5e1"), json!(-25)),
        (json!("0xff"), json!(255)),
        (json!("-0x10"), json!(-16)),
        (json!("abc"), json!(null)),
        (json!("1abc"), json!(null)),
        // Looser than JSON: a leading sign, leading zeros and a trailing point.
        (json!("+1"), json!(1)),
        (json!("01"), json!(1)),
        (json!("1."), json!(1)),
        (json!(".5"), json!(0.5)),
        (json!("0o17"), json!(15)),
        (json!("1,000"), json!(null)),
        (json!("1 000"), json!(null)),
        (json!("--1"), json!(null)),
        (json!([]), json!(null)),
        (json!({}), json!(null)),
        (json!([1]), json!(null)),
    ];
    for (input, expected) in cases {
        assert_eq!(
            &run("loose_number", slice::from_ref(input)),
            expected,
            "loose_number({input})"
        );
    }
}

/// Truthiness: `false`, `0`, `-0`, `""`, `null`, NaN are falsy. Empty
/// containers are **truthy**, the opposite of this crate's own rule.
#[test]
fn loose_truthy_matrix() {
    for falsy in [json!(false), json!(0), json!(-0.0), json!(""), json!(null)] {
        assert_eq!(
            run("loose_truthy", slice::from_ref(&falsy)),
            json!(false),
            "{falsy}"
        );
    }
    for truthy in [
        json!(true),
        json!(1),
        json!(-1),
        json!("0"),
        json!("false"),
        json!([]),
        json!({}),
        json!([0]),
    ] {
        assert_eq!(
            run("loose_truthy", slice::from_ref(&truthy)),
            json!(true),
            "{truthy}"
        );
    }
    // NaN arrives as a string that coerces to NaN, since JSON has no NaN.
    let mut t = ExprTable::new();
    let nan = call(&mut t, "loose_number", &[json!("abc")]);
    let truthy = t.call("loose_truthy", vec![nan]);
    assert_eq!(
        eval(&t, truthy, &empty().env()).unwrap(),
        json!(false),
        "NaN is falsy"
    );
}

/// Equality: same kinds compare directly (strings case-insensitively);
/// different kinds coerce to numbers; NaN equals nothing; containers never
/// equal.
#[test]
fn loose_equality_matrix() {
    let equal: &[(Value, Value)] = &[
        (json!(null), json!(null)),
        (json!(null), json!(0)),
        (json!(null), json!("0")),
        (json!(null), json!("")),
        (json!(null), json!(false)),
        (json!(true), json!(1)),
        (json!(false), json!(0)),
        (json!("1"), json!(1)),
        (json!("0xff"), json!(255)),
        (json!("Hello"), json!("hello")),
        (json!("ÉCOLE"), json!("école")),
        (json!(1), json!(1.0)),
        (json!(""), json!(0)),
        (json!(" 7 "), json!(7)),
    ];
    for (a, b) in equal {
        assert_eq!(
            run("loose_eq", &[a.clone(), b.clone()]),
            json!(true),
            "{a} == {b}"
        );
        assert_eq!(
            run("loose_eq", &[b.clone(), a.clone()]),
            json!(true),
            "{b} == {a}"
        );
    }
    let unequal: &[(Value, Value)] = &[
        (json!("abc"), json!(0)),
        (json!("abc"), json!(null)),
        (json!("abc"), json!("abd")),
        (json!([]), json!([])),
        (json!({}), json!({})),
        (json!([]), json!(0)),
        (json!({}), json!(null)),
        (json!(true), json!("true")),
        (json!(1), json!(2)),
        (json!("1"), json!("01")),
    ];
    for (a, b) in unequal {
        assert_eq!(
            run("loose_eq", &[a.clone(), b.clone()]),
            json!(false),
            "{a} != {b}"
        );
    }
}

/// Relational: two strings compare as strings, case-insensitively; anything
/// else as numbers; NaN makes every comparison false.
#[test]
fn loose_relational_matrix() {
    assert_eq!(run("loose_lt", &[json!(1), json!(2)]), json!(true));
    assert_eq!(run("loose_lt", &[json!("1"), json!(2)]), json!(true));
    assert_eq!(
        run("loose_lt", &[json!("10"), json!("9")]),
        json!(true),
        "two strings compare as strings"
    );
    assert_eq!(
        run("loose_lt", &[json!("a"), json!("B")]),
        json!(true),
        "case-insensitively"
    );
    assert_eq!(run("loose_le", &[json!(null), json!(0)]), json!(true));
    assert_eq!(run("loose_ge", &[json!(true), json!(1)]), json!(true));
    assert_eq!(
        run("loose_gt", &[json!("abc"), json!(0)]),
        json!(false),
        "NaN: false"
    );
    assert_eq!(
        run("loose_lt", &[json!("abc"), json!(0)]),
        json!(false),
        "NaN: false both ways"
    );
    assert_eq!(
        run("loose_le", &[json!([]), json!([])]),
        json!(false),
        "arrays coerce to NaN"
    );
}

#[test]
fn loose_string_coercion() {
    assert_eq!(run("loose_string", &[json!(null)]), json!(""));
    assert_eq!(run("loose_string", &[json!(true)]), json!("true"));
    assert_eq!(run("loose_string", &[json!(3)]), json!("3"));
    assert_eq!(run("loose_string", &[json!(3.0)]), json!("3"));
    assert_eq!(run("loose_string", &[json!(2.5)]), json!("2.5"));
    assert_eq!(run("loose_string", &[json!([1])]), json!("Array"));
    assert_eq!(run("loose_string", &[json!({"a": 1})]), json!("Object"));
}

#[test]
fn loose_string_functions() {
    assert_eq!(
        run("contains_ci", &[json!("Hello World"), json!("WORLD")]),
        json!(true)
    );
    assert_eq!(
        run("contains_ci", &[json!(["a", "B"]), json!("b")]),
        json!(true),
        "array membership is loose"
    );
    assert_eq!(
        run("contains_ci", &[json!([1, 2]), json!("2")]),
        json!(true)
    );
    assert_eq!(
        run("contains_ci", &[json!({"a": 1}), json!("a")]),
        json!(false),
        "objects: false"
    );
    assert_eq!(run("contains_ci", &[json!("abc"), json!([])]), json!(false));
    assert_eq!(
        run("starts_with", &[json!("Hello"), json!("HE")]),
        json!(true)
    );
    assert_eq!(
        run("ends_with", &[json!("Hello"), json!("LO")]),
        json!(true)
    );
    assert_eq!(
        run("starts_with", &[json!(123), json!("12")]),
        json!(true),
        "coerces to strings"
    );
    assert_eq!(run("starts_with", &[json!([]), json!("")]), json!(false));
}

#[test]
fn format_and_join() {
    assert_eq!(
        run("format", &[json!("{0} and {1}"), json!(["a", 2])]),
        json!("a and 2")
    );
    assert_eq!(
        run("format", &[json!("{{literal}} {0}"), json!([true])]),
        json!("{literal} true")
    );
    assert_eq!(run("format", &[json!("{0}"), json!([null])]), json!(""));
    let mut t = ExprTable::new();
    let bad = call(&mut t, "format", &[json!("{0} {1}"), json!(["only one"])]);
    assert!(
        matches!(eval(&t, bad, &empty().env()), Err(EvalError::Type { .. })),
        "missing argument is an error"
    );

    assert_eq!(
        run("join", &[json!(["a", 1, true]), json!(null)]),
        json!("a,1,true"),
        "default separator"
    );
    assert_eq!(
        run("join", &[json!(["a", "b"]), json!(" | ")]),
        json!("a | b")
    );
    assert_eq!(
        run("join", &[json!("solo"), json!(",")]),
        json!("solo"),
        "a non-array is just stringified"
    );
}

#[test]
fn json_functions() {
    assert_eq!(
        run("from_json", &[json!(r#"{"a":[1,2]}"#)]),
        json!({"a": [1, 2]})
    );
    assert_eq!(
        run("from_json", &[json!({"already": true})]),
        json!({"already": true}),
        "non-strings pass through"
    );
    let mut t = ExprTable::new();
    let bad = call(&mut t, "from_json", &[json!("{not json")]);
    assert!(matches!(
        eval(&t, bad, &empty().env()),
        Err(EvalError::Type { .. })
    ));
    let text = run("to_json", &[json!({"a": 1})]);
    assert_eq!(
        serde_json::from_str::<Value>(text.as_str().unwrap()).unwrap(),
        json!({"a": 1})
    );
}

#[test]
fn record_access_functions() {
    let obj = json!({"Event_Name": "push", "ref": "main"});
    assert_eq!(
        run("get_ci", &[obj.clone(), json!("event_name")]),
        json!("push")
    );
    assert_eq!(run("get_ci", &[obj.clone(), json!("REF")]), json!("main"));
    assert_eq!(run("get_ci", &[obj.clone(), json!("missing")]), json!(null));
    assert_eq!(
        run("get_ci", &[json!([1]), json!("x")]),
        json!(null),
        "not an object: null"
    );

    assert_eq!(run("values", &[json!({"a": 1, "b": 2})]), json!([1, 2]));
    assert_eq!(run("values", &[json!([1, 2])]), json!([1, 2]));
    assert_eq!(run("values", &[json!("x")]), json!(null));

    // `pluck_present` skips records lacking the key and keeps present keys, even
    // null-valued ones — the difference from `pluck`.
    let commits = json!([{"message": "a"}, {"other": 1}, {"message": null}, "not an object"]);
    assert_eq!(
        run("pluck_present", &[commits, json!("message")]),
        json!(["a", null])
    );
}

/// The record combinators a frontend composes a matrix from.
#[test]
fn record_combinators() {
    // Cartesian: first key varies slowest; a scalar is a one-value axis.
    assert_eq!(
        run("cartesian", &[json!({"os": ["a", "b"], "v": [1, 2]})]),
        json!([{"os": "a", "v": 1}, {"os": "a", "v": 2}, {"os": "b", "v": 1}, {"os": "b", "v": 2}])
    );
    assert_eq!(
        run("cartesian", &[json!({"os": "solo", "v": [1]})]),
        json!([{"os": "solo", "v": 1}])
    );
    assert_eq!(run("cartesian", &[json!(null)]), json!([]));
    assert_eq!(run("cartesian", &[json!({})]), json!([]));

    // keys / omit.
    assert_eq!(
        run("keys", &[json!({"b": 1, "a": 2})]),
        json!(["b", "a"]),
        "document order"
    );
    assert_eq!(
        run("omit", &[json!({"a": 1, "b": 2, "c": 3}), json!(["b"])]),
        json!({"a": 1, "c": 3})
    );
    assert_eq!(
        run("omit", &[json!("scalar"), json!(["b"])]),
        json!("scalar")
    );

    // reject_where: any partial that matches removes the record; null removes
    // nothing.
    let records = json!([{"os": "a", "v": 1}, {"os": "a", "v": 2}, {"os": "b", "v": 1}]);
    assert_eq!(
        run("reject_where", &[
            records.clone(),
            json!([{"os": "a", "v": 2}])
        ]),
        json!([{"os": "a", "v": 1}, {"os": "b", "v": 1}])
    );
    assert_eq!(
        run("reject_where", &[records.clone(), json!([{"os": "a"}])]),
        json!([{"os": "b", "v": 1}])
    );
    assert_eq!(
        run("reject_where", &[records.clone(), json!(null)]),
        records
    );

    // extend_where: compatible partials merge their unprotected keys into every
    // compatible record; incompatible ones are appended; appended records are never
    // themselves extended.
    let base = json!([{"fruit": "apple", "animal": "cat"}, {"fruit": "pear", "animal": "dog"}]);
    let partials = json!([
        {"color": "green"},
        {"color": "pink", "animal": "cat"},
        {"fruit": "banana"},
        {"fruit": "banana", "animal": "cat"},
    ]);
    assert_eq!(
        run("extend_where", &[
            base,
            partials,
            json!(["fruit", "animal"])
        ]),
        json!([
            {"fruit": "apple", "animal": "cat", "color": "pink"},
            {"fruit": "pear", "animal": "dog", "color": "green"},
            {"fruit": "banana"},
            {"fruit": "banana", "animal": "cat"},
        ])
    );
    assert_eq!(
        run("extend_where", &[
            json!([{"a": 1}]),
            json!(null),
            json!(["a"])
        ]),
        json!([{"a": 1}])
    );
}
