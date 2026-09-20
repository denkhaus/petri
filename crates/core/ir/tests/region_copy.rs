//! Copying a region of one graph into a fresh builder: every expression is
//! imported once, and an edge that leaves the region is dropped.

use std::collections::HashMap;

use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    Arm, BinOp, ExpandTarget, Expansion, Expr, ExprOrValue, ExprTable, GraphBuilder, Guard,
    ScopeId, StepKindId, StepRef,
};
use serde_json::json;

const NOOP: StepKindId = StepKindId::new_static("noop");

#[test]
fn an_import_copies_a_shared_subexpression_once_and_remembers_it() {
    let mut from: ExprTable = ExprTable::new();
    let x = from.var("x");
    let one = from.lit(1);
    let sum = from.binary(BinOp::Add, x, one);
    let square = from.binary(BinOp::Mul, sum, sum);

    let mut to: ExprTable = ExprTable::new();
    let mut imported = HashMap::new();
    let copied = to.import(&from, square, &mut imported);

    // `x`, `1`, the sum and the square: the shared sum is one entry.
    assert_eq!(to.len(), 4);
    let Some(Expr::Binary(BinOp::Mul, lhs, rhs)) = to.get(copied).cloned() else {
        panic!("the copy is the product");
    };
    assert_eq!(lhs, rhs);
    assert!(matches!(to.get(lhs), Some(Expr::Binary(BinOp::Add, _, _))));
    // The same id imported again is the same copy.
    assert_eq!(to.import(&from, sum, &mut imported), lhs);
    assert_eq!(to.len(), 4);
}

#[test]
fn a_region_copy_keeps_what_is_inside_and_drops_the_edges_that_leave() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let env = b.exprs().var("home");
    b.graph_mut().body.scopes[0]
        .env
        .insert("HOME".into(), ExprOrValue::Expr(env));
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    let out = b.add_step("out", scope, NOOP);

    let kv = b.exprs().var("kv");
    b.node_mut(a).step = StepRef::new(NOOP, json!({ "kv": { EXPR_PLACEHOLDER_KEY: kv.raw() } }));
    let pre = b.exprs().lit(true);
    b.set_precondition(a, pre);
    let guard = {
        let exprs = b.exprs();
        let status = exprs.var("status");
        let ok = exprs.lit("ok");
        exprs.binary(BinOp::Eq, status, ok)
    };
    let payload = b.exprs().var("output");
    b.select(a, vec![
        Arm::when(c, guard).with_map(payload),
        Arm::always(out),
    ]);
    let items = b.exprs().var("list");
    b.set_expansion(c, Expansion::ForEach {
        items,
        target: ExpandTarget::Node,
        max_parallel: Some(2),
        fail_fast: false,
    });
    b.link(c, out);
    let parent = b.build();

    let (copy, copies) = GraphBuilder::copy_region(&parent, &[a, c]);
    let copy = copy.build();
    assert_eq!(copy.nodes.len(), 2);
    assert_eq!(copies.len(), 2);

    // The scope's environment expression is in the new table.
    let Some(ExprOrValue::Expr(env)) = copy.scopes[0].env.get("HOME") else {
        panic!("the scope keeps its environment");
    };
    assert_eq!(copy.exprs.get(*env), Some(&Expr::Var("home".into())));

    let a = copy.node(copies[&a]).expect("`a` is copied");
    let kv = a.step.config["kv"][EXPR_PLACEHOLDER_KEY]
        .as_u64()
        .expect("the placeholder is rewritten");
    assert_eq!(
        copy.exprs
            .get(ir::ExprId::new(u32::try_from(kv).expect("an id"))),
        Some(&Expr::Var("kv".into()))
    );
    let pre = a.precondition.expect("the precondition comes along");
    assert_eq!(copy.exprs.get(pre), Some(&Expr::Lit(true.into())));
    // The guarded, mapped arm to `c` survives; the arm to `out` is gone.
    let arms: Vec<_> = a.routing.edges().collect();
    assert_eq!(arms.len(), 1);
    assert_eq!(arms[0].to, copies[&c]);
    let Guard::Expr(guard) = arms[0].guard else {
        panic!("the guard comes along");
    };
    assert!(matches!(
        copy.exprs.get(guard),
        Some(Expr::Binary(BinOp::Eq, _, _))
    ));
    let map = arms[0].map.expect("the map comes along");
    assert_eq!(copy.exprs.get(map), Some(&Expr::Var("output".into())));

    let c = copy.node(copies[&c]).expect("`c` is copied");
    let Some(Expansion::ForEach {
        items,
        max_parallel,
        ..
    }) = &c.expand
    else {
        panic!("the expansion comes along");
    };
    assert_eq!(copy.exprs.get(*items), Some(&Expr::Var("list".into())));
    assert_eq!(*max_parallel, Some(2));
    // Its only edge left the region, so it routes nowhere.
    assert_eq!(c.routing.edges().count(), 0);
}
