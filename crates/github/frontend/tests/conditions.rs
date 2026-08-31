//! Job-level `if:` semantics, pinned end to end: a user-written comparison
//! lowers onto the loose builtins (GitHub's coercion), while the comparisons
//! petri builds itself — the `success()` admission against engine-written
//! status tags — stay strict. The shape is uv's `rebase-and-push` gate,
//! `needs.identify.outputs.rebasable != '0'`, whose skip depends on GitHub's
//! null-coerces-to-number rule.

mod support;

use frontend::print;
use ir::{EvalEnv, NodeRecord, RunContext, StaticCtx, Status, eval};
use serde_json::{Value, json};
use smol_str::SmolStr;
use support::*;

const TEXT: &str = r"
on: push
jobs:
  identify:
    runs-on: ubuntu-latest
    outputs:
      rebasable: ${{ steps.identify.outputs.rebasable }}
    steps:
      - id: identify
        run: echo ok
  rebase:
    needs: identify
    if: needs.identify.outputs.rebasable != '0'
    runs-on: ubuntu-latest
    steps:
      - run: echo go
";

fn rebase_precondition(graph: &ir::Graph) -> ir::ExprId {
    graph
        .nodes
        .iter()
        .find(|n| n.name == "rebase/start")
        .expect("rebase/start")
        .precondition
        .expect("a job `if:` is an engine precondition")
}

/// The user's `!=` lowers to the loose builtin; the admission petri prepends
/// compares engine-written tags strictly. Both are visible in the printed tree.
#[test]
fn user_comparisons_lower_loose_and_internal_ones_stay_strict() {
    let graph = lower_ok(TEXT);
    let pre = rebase_precondition(&graph);
    let printed = print::print_expr(&graph.exprs, pre);
    assert!(
        printed.contains("loose_eq("),
        "the user comparison is loose: {printed}"
    );
    assert!(
        printed.contains("== 'success'"),
        "the internal admission stays strict: {printed}"
    );
}

/// The gate through the engine's evaluator, over what `identify` reported. A
/// missing output reads as null, and GitHub coerces null and '0' to the same
/// number, so `!= '0'` skips — while an empty string compares as a string and
/// admits, on GitHub and here alike.
#[test]
fn a_needs_output_gate_follows_github_coercion() {
    let graph = lower_ok(TEXT);
    let pre = rebase_precondition(&graph);
    let cases: &[(Value, bool)] = &[
        (json!(null), false),
        (json!("0"), false),
        (json!(0), false),
        (json!(""), true),
        (json!("3"), true),
    ];
    for (rebasable, admitted) in cases {
        let mut run = RunContext::new();
        run.record(SmolStr::new("identify/done"), NodeRecord {
            status:     Status::Success,
            output:     json!({ "result": "success", "outputs": { "rebasable": rebasable } }),
            generation: ir::Generation::ZERO,
            attempts:   1,
        });
        let statics = StaticCtx::new().bind("scope_cancelled", json!(false));
        let token = Value::Null;
        let env = EvalEnv::new(&token, &run, &statics);
        let got = eval(&graph.exprs, pre, &env).expect("the gate evaluates");
        assert_eq!(got, json!(*admitted), "rebasable = {rebasable}");
    }
}
