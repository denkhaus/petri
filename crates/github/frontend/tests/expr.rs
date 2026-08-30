//! Handoff §7 tests 1–2 for the GHA lowering specifically: GitHub's semantics
//! come out of the engine's evaluator, every GitHub function has a table home,
//! and the matrix rules are a composition of engine combinators.

use frontend::expr::lower::{EngineBindings, Roots};
use frontend::expr::parse;
use frontend_gha::expr_lower::{
    GHA_FUNCTIONS, GHA_FUNCTIONS_UNSUPPORTED, gha, gha_function, matrix_legs,
};
use ir::{EvalEnv, ExprTable, RunContext, StaticCtx, eval, expr as ir_expr};
use serde_json::json;

fn eval_gha(source: &str, statics: &StaticCtx) -> serde_json::Value {
    struct Ctx;
    impl Roots for Ctx {
        fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ir::ExprId> {
            Some(table.var(name))
        }
    }
    let ast = parse(source).unwrap_or_else(|e| panic!("{source}: {e}"));
    let mut table = ExprTable::new();
    let id = gha(&ast, &mut table, &mut Ctx).unwrap_or_else(|e| panic!("{source}: {e}"));
    let run = RunContext::new();
    let token = serde_json::Value::Null;
    eval(&table, id, &EvalEnv::new(&token, &run, statics))
        .unwrap_or_else(|e| panic!("{source}: {e}"))
}

/// GitHub's semantics, checked by running the lowered expression through the
/// engine — the frontend has no evaluator of its own.
#[test]
fn gha_lowering_produces_github_semantics() {
    let mut statics = StaticCtx::new();
    statics.set(
        "a",
        json!({"Name": "x", "list": [{"k": 1}, {"k": 2}, {"other": 3}], "n": "3"}),
    );
    statics.set("empty", json!([]));

    let cases: &[(&str, serde_json::Value)] = &[
        ("a.n == 3", json!(true)),
        ("'ABC' == 'abc'", json!(true)),
        ("null == 0", json!(true)),
        ("null != '0'", json!(false)),
        ("'' == 0", json!(true)),
        ("'abc' == 0", json!(false)),
        ("a.n < 10", json!(true)),
        ("a.n && 'yes'", json!("yes")),
        ("null && 'yes'", json!(null)),
        ("null || 'fallback'", json!("fallback")),
        ("'first' || 'second'", json!("first")),
        ("!empty", json!(false)),
        ("!''", json!(true)),
        ("a.name", json!("x")),
        ("a.NAME", json!("x")),
        ("a['nAmE']", json!("x")),
        ("a.list.*.k", json!([1, 2])),
        ("a.list.k", json!(null)),
        ("contains(a.name, 'X')", json!(true)),
        ("format('{0}={1}', 'a', a.n)", json!("a=3")),
        ("join(a.list.*.k, '+')", json!("1+2")),
        (
            "startsWith(a.name, 'X') && endsWith('hello', 'LO')",
            json!(true),
        ),
        ("fromJSON('{\"z\": [1]}').z[0]", json!(1)),
        ("a.missing.deeper", json!(null)),
    ];
    for (source, expected) in cases {
        assert_eq!(&eval_gha(source, &statics), expected, "{source}");
    }
}

/// Every GitHub function has a home in the table, and the one that cannot is
/// named with its reason. This is the "no frontend evaluator path" assertion in
/// mechanical form: the frontend produces `ir::Expr` and nothing else.
#[test]
fn every_gha_function_has_a_home() {
    for name in GHA_FUNCTIONS {
        let mut table = ExprTable::new();
        let arity = match *name {
            "always" | "success" | "failure" | "cancelled" => 0,
            "toJSON" | "fromJSON" => 1,
            _ => 2,
        };
        let args: Vec<_> = (0..arity).map(|_| table.lit(1)).collect();
        gha_function(name, args, &mut table).unwrap_or_else(|e| panic!("{name}: {e}"));
        for (_, expr) in table.iter() {
            if let ir::Expr::Call(fname, _) = expr {
                assert!(
                    ir_expr::builtin(fname).is_some(),
                    "{name} lowered to `{fname}`, not in BUILTINS"
                );
            }
        }
    }
    assert!(
        GHA_FUNCTIONS_UNSUPPORTED
            .iter()
            .any(|(n, _)| *n == "hashFiles")
    );
    let mut table = ExprTable::new();
    assert!(gha_function("hashFiles", vec![], &mut table).is_err());
    assert!(gha_function("definitelyNot", vec![], &mut table).is_err());
}

/// Whatever the GHA lowering emits is in the table — over a spread of shapes.
#[test]
fn gha_lowering_emits_only_table_functions() {
    for source in [
        "a == b",
        "a != b && c < d || !e",
        "contains(a, 'x') && startsWith(b, 'y')",
        "format('{0}', a, b, c)",
        "join(a)",
        "toJSON(a) == fromJSON(b)",
        "a.b.c",
        "a['b'][0]",
        "a.*.b",
        "a.*.*",
        "a.b.*.c[1]",
        "always() || success() || failure() || cancelled()",
    ] {
        let ast = parse(source).unwrap();
        let mut table = ExprTable::new();
        gha(&ast, &mut table, &mut EngineBindings).unwrap_or_else(|e| panic!("{source}: {e}"));
        for (_, expr) in table.iter() {
            if let ir::Expr::Call(fname, args) = expr {
                let spec = ir_expr::builtin(fname)
                    .unwrap_or_else(|| panic!("`{source}` emitted `{fname}`"));
                assert_eq!(spec.arity, args.len(), "`{fname}` in `{source}`");
            }
        }
    }
}

/// GitHub's documented matrix examples, through the composition of engine
/// combinators the frontend emits. The engine knows nothing about `include`.
#[test]
fn matrix_rules_follow_githubs_documented_examples() {
    let expand = |matrix: serde_json::Value| {
        let mut table = ExprTable::new();
        let m = table.lit(matrix);
        let legs = matrix_legs(&mut table, m).unwrap();
        let run = RunContext::new();
        let statics = StaticCtx::new();
        eval(
            &table,
            legs,
            &EvalEnv::new(&serde_json::Value::Null, &run, &statics),
        )
        .unwrap()
    };

    assert_eq!(
        expand(json!({"os": ["ubuntu", "windows"], "node": [14, 16]})),
        json!([
            {"os": "ubuntu", "node": 14}, {"os": "ubuntu", "node": 16},
            {"os": "windows", "node": 14}, {"os": "windows", "node": 16},
        ])
    );

    // The documented include example.
    assert_eq!(
        expand(json!({
            "fruit": ["apple", "pear"],
            "animal": ["cat", "dog"],
            "include": [
                {"color": "green"},
                {"color": "pink", "animal": "cat"},
                {"fruit": "apple", "shape": "circle"},
                {"fruit": "banana"},
                {"fruit": "banana", "animal": "cat"},
            ]
        })),
        json!([
            {"fruit": "apple", "animal": "cat", "color": "pink", "shape": "circle"},
            {"fruit": "apple", "animal": "dog", "color": "green", "shape": "circle"},
            {"fruit": "pear", "animal": "cat", "color": "pink"},
            {"fruit": "pear", "animal": "dog", "color": "green"},
            {"fruit": "banana"},
            {"fruit": "banana", "animal": "cat"},
        ])
    );

    // Exclude runs before include, so an include can add a leg back.
    assert_eq!(
        expand(json!({
            "os": ["a", "b"], "v": [1, 2],
            "exclude": [{"os": "a", "v": 2}],
            "include": [{"os": "a", "v": 2, "special": true}]
        })),
        json!([{"os": "a", "v": 1}, {"os": "b", "v": 1}, {"os": "b", "v": 2}, {"os": "a", "v": 2, "special": true}])
    );

    // Include only; and a non-object is no legs.
    assert_eq!(
        expand(json!({"include": [{"a": 1}, {"a": 2}]})),
        json!([{"a": 1}, {"a": 2}])
    );
    assert_eq!(expand(json!(null)), json!([]));
}
