//! Acceptance §7 item 3, the pure half: what lowering produces — routing
//! tiers, failure policies, goal gates, parallel, budgets, and the specific
//! rejections. The run half is `crates/fabro/acceptance/tests/routing.rs`.

mod support;

use std::time::Duration;

use frontend::CompileInputs;
use frontend::print::print_expr;
use frontend_fabro::MAX_FIRINGS;
use frontend_fabro::kinds::{AGENT_KIND, COMMAND_KIND, HUMAN_KIND, WAIT_KIND, WORKFLOW_KIND};
use ir::placeholder::contains_placeholder;
use ir::{Completion, EdgeTransition, Exhaustion, Guard, JoinPolicy, PickPolicy};
use serde_json::json;
use support::*;

#[test]
fn shapes_lower_to_their_step_kinds() {
    let graph = lower_ok(&dot(r#"
        a [label="Agent", prompt="do"]
        p [label="Prompt", shape=tab, prompt="say"]
        c [label="Cmd", shape=parallelogram, script="true"]
        inferred [label="Inferred", script="true"]
        h [shape=hexagon, label="Ok?"]
        w [shape=insulator, duration="1s"]
        d [shape=diamond]
        m [shape=house, stack.child_dot_source="digraph C { start [shape=Mdiamond] exit [shape=Msquare] start -> exit }", manager.max_cycles=3]
        start -> a -> p -> c -> inferred -> h
        h -> w [label="[Y] Yes"]
        w -> d -> m -> exit
    "#));
    assert_eq!(node(&graph, "start").step.kind.as_str(), "noop");
    assert_eq!(node(&graph, "exit").step.kind.as_str(), "noop");
    assert_eq!(node(&graph, "d").step.kind.as_str(), "noop");
    assert_eq!(node(&graph, "a").step.kind, AGENT_KIND);
    assert_eq!(node(&graph, "p").step.kind, AGENT_KIND);
    assert_eq!(node(&graph, "p").step.config["kind"], json!("prompt"));
    assert_eq!(node(&graph, "c").step.kind, COMMAND_KIND);
    assert_eq!(node(&graph, "inferred").step.kind, COMMAND_KIND);
    assert_eq!(node(&graph, "h").step.kind, HUMAN_KIND);
    assert_eq!(node(&graph, "w").step.kind, WAIT_KIND);
    assert_eq!(node(&graph, "w").step.config["duration_ms"], json!(1000));
    assert_eq!(node(&graph, "m").step.kind, WORKFLOW_KIND);
    assert!(node(&graph, "m").step.config["child_digest"].is_string());
    assert_eq!(
        graph.completion,
        Completion::TerminalNode(node_id(&graph, "exit"))
    );
    assert_eq!(graph.entry, vec![node_id(&graph, "start")]);
    assert_eq!(node(&graph, "a").meta["label"], json!("Agent"));
    assert_eq!(node(&graph, "a").meta["shape"], json!("box"));
    ir::validate(&graph).expect("validates");
}

#[test]
fn the_four_tiers_lower_in_order_with_their_picks() {
    let graph = lower_ok(&dot(r#"
        a [prompt="x"]
        b [prompt="x"]
        c [prompt="x"]
        d [prompt="x"]
        start -> a
        a -> b [condition="outcome=succeeded", weight=2]
        a -> c [label="[F] Fix"]
        a -> d
        b -> exit
        c -> exit
        d -> exit
    "#));
    let tiers = tiers(&graph, "a");
    assert_eq!(tiers.len(), 4);
    assert_eq!(tiers[0].0, PickPolicy::HighestWeightThenLexical);
    assert_eq!(
        tiers[0]
            .1
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );
    assert_eq!(tiers[1].0, PickPolicy::First);
    assert_eq!(
        tiers[1]
            .1
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["c"]
    );
    assert_eq!(tiers[2].0, PickPolicy::LowestRankThenArmOrder);
    assert_eq!(
        tiers[2]
            .1
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["c", "d"]
    );
    assert_eq!(tiers[3].0, PickPolicy::HighestWeightThenLexical);
    assert_eq!(
        tiers[3]
            .1
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["c", "d"]
    );
    assert!(
        matches!(tiers[3].1[0].1, Guard::Always),
        "route: the fallback is unconditional"
    );

    // Tier 1 fires on the outcome, tier 2 on the normalized label, tier 3 on
    // the suggested id.
    let ok = statics("success", &json!({}));
    assert!(eval_guard(&graph, tiers[0].1[0].1, &ok, &[]));
    let failed = statics("failure", &json!({}));
    assert!(!eval_guard(&graph, tiers[0].1[0].1, &failed, &[]));
    let labelled = statics("success", &json!({ "preferred_label": "fix" }));
    assert!(eval_guard(&graph, tiers[1].1[0].1, &labelled, &[]));
    let other = statics("success", &json!({ "preferred_label": "Approve" }));
    assert!(!eval_guard(&graph, tiers[1].1[0].1, &other, &[]));
    let suggested = statics("success", &json!({ "suggested_next_ids": ["d", "c"] }));
    assert!(eval_guard(&graph, tiers[2].1[0].1, &suggested, &[]));
    assert!(eval_guard(&graph, tiers[2].1[1].1, &suggested, &[]));
    let weights: Vec<u32> = node(&graph, "a").routing.groups[0]
        .arms
        .iter()
        .map(|a| a.weight)
        .collect();
    assert_eq!(weights, [2, 0, 0]);
    assert_eq!(
        node(&graph, "a").routing.groups[0].arms[1].label.as_deref(),
        Some("[F] Fix")
    );
}

#[test]
fn random_selection_picks_weighted_and_forbids_conditions() {
    let graph = lower_ok(&dot(r#"
        graph [selection="random"]
        a [prompt="x"]
        b [prompt="x"]
        start -> a
        a -> b [weight=3]
        a -> exit
        b -> exit
    "#));
    let tiers = tiers(&graph, "a");
    assert_eq!(
        tiers.last().expect("fallback").0,
        PickPolicy::WeightedRandom
    );
    let weights: Vec<u32> = node(&graph, "a").routing.groups[0]
        .arms
        .iter()
        .map(|a| a.weight)
        .collect();
    assert_eq!(
        weights,
        [3, 1],
        "a zero weight counts as one under random selection"
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", selection="random"]
        start -> a
        a -> exit [condition="outcome=succeeded"]
        a -> exit
    "#))
        .contains(&"fabro.random_with_conditions".to_string())
    );
}

#[test]
fn failure_policies_guard_the_fallback_tier() {
    // route: always; exit: only a non-failed outcome; a human gate never falls
    // through on failure whatever its policy says.
    let graph = lower_ok(&dot(r#"
        graph [on_failure="exit"]
        r [prompt="x", on_failure="route"]
        e [prompt="x"]
        h [shape=hexagon, on_failure="route"]
        x [prompt="x", on_failure="route", on_retries_exhausted="exit", max_retries=1]
        start -> r -> e -> h
        h -> x [label="Go"]
        x -> exit
    "#));
    let fallback = |name: &str| tiers(&graph, name).last().expect("fallback").1[0].1;
    assert!(matches!(fallback("r"), Guard::Always));
    let failed = statics("failure", &json!({ "failure_class": "" }));
    let ok = statics("success", &json!({}));
    let skipped = statics("skipped", &json!({}));
    let timed_out = statics("timed_out", &json!({}));
    assert!(!eval_guard(&graph, fallback("e"), &failed, &[]));
    assert!(!eval_guard(&graph, fallback("e"), &timed_out, &[]));
    assert!(eval_guard(&graph, fallback("e"), &ok, &[]));
    assert!(eval_guard(&graph, fallback("e"), &skipped, &[]));
    assert!(!eval_guard(&graph, fallback("h"), &failed, &[]));
    assert!(eval_guard(&graph, fallback("h"), &ok, &[]));
    // Mixed: a non-retryable failure routes, an exhausted retryable one exits.
    let exhausted = statics("failure", &json!({ "failure_class": "retry_requested" }));
    assert!(eval_guard(&graph, fallback("x"), &failed, &[]));
    assert!(!eval_guard(&graph, fallback("x"), &exhausted, &[]));
    assert_eq!(node(&graph, "x").retry.max_attempts.get(), 2);
    assert_eq!(node(&graph, "x").retry.on_exhaustion, Exhaustion::Fail);
}

#[test]
fn partially_succeed_and_allow_partial_lower_to_accept_partial() {
    let graph = lower_ok(&dot(r#"
        a [prompt="x", allow_partial=true, max_retries=2]
        b [prompt="x", on_retries_exhausted="partially_succeed"]
        c [prompt="x", on_failure="partially_succeed"]
        start -> a -> b -> c -> exit
    "#));
    assert_eq!(
        node(&graph, "a").retry.on_exhaustion,
        Exhaustion::AcceptPartial
    );
    assert_eq!(node(&graph, "a").retry.max_attempts.get(), 3);
    assert_eq!(
        node(&graph, "b").retry.on_exhaustion,
        Exhaustion::AcceptPartial
    );
    assert_eq!(
        node(&graph, "c").step.config["on_failure"],
        json!("partially_succeed")
    );
    assert_eq!(
        node(&graph, "c").retry.on_exhaustion,
        Exhaustion::AcceptPartial
    );
    assert_eq!(node(&graph, "a").step.config["on_failure"], json!("route"));
    let retry_on = &node(&graph, "a").retry.retry_on;
    assert!(retry_on.statuses.is_empty());
    assert_eq!(retry_on.failure_classes, vec![ir::FailureClass::new(
        "retry_requested"
    )]);
}

#[test]
fn the_unreachable_failure_edge_is_linted() {
    let diags = diagnostics(&dot(r#"
        a [prompt="x", on_failure="partially_succeed"]
        b [prompt="x"]
        start -> a
        a -> b [condition="outcome=failed"]
        a -> exit
        b -> exit
    "#));
    assert!(
        diags
            .iter()
            .any(|d| d.code == "fabro.unreachable_failure_edge"),
        "{diags:?}"
    );
}

#[test]
fn deprecated_success_spellings_are_rejected_with_specific_codes() {
    assert!(
        codes(&dot(r#"
        a [prompt="x", on_failure="succeed"]
        start -> a -> exit
    "#))
        .contains(&"unsupported.on_failure.succeed".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", auto_status=true]
        start -> a -> exit
    "#))
        .contains(&"unsupported.auto_status".to_string())
    );
    assert!(
        codes(&dot(r#"
        graph [on_failure="stop"]
        a [prompt="x"]
        start -> a -> exit
    "#))
        .contains(&"fabro.bad_on_failure".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", allow_partial=true, on_retries_exhausted="exit"]
        start -> a -> exit
    "#))
        .contains(&"fabro.allow_partial_conflict".to_string())
    );
}

#[test]
fn retry_presets_and_defaults_lower_to_retry_policies() {
    let graph = lower_ok(&dot(r#"
        graph [default_max_retries=3]
        d [prompt="x"]
        n [prompt="x", max_retries=0]
        s [prompt="x", retry_policy="standard"]
        l [prompt="x", retry_policy="linear"]
        start -> d -> n -> s -> l -> exit
    "#));
    assert_eq!(node(&graph, "d").retry.max_attempts.get(), 4);
    assert_eq!(node(&graph, "n").retry.max_attempts.get(), 1);
    assert_eq!(node(&graph, "s").retry.max_attempts.get(), 5);
    assert_eq!(node(&graph, "l").retry.max_attempts.get(), 3);
    assert!((node(&graph, "l").retry.backoff.factor - 1.0).abs() < f64::EPSILON);
    assert_eq!(
        node(&graph, "l").retry.backoff.initial,
        Duration::from_millis(500)
    );
    assert_eq!(
        node(&graph, "d").retry.backoff.initial,
        Duration::from_secs(5)
    );
}

#[test]
fn goal_gates_insert_a_check_with_back_arms_in_resolution_order() {
    let graph = lower_ok(&dot(r#"
        graph [retry_target="plan"]
        plan [prompt="x"]
        work [prompt="x", goal_gate=true, retry_target="missing", fallback_retry_target="work"]
        verify [prompt="x", goal_gate=true]
        start -> plan -> work -> verify -> exit
    "#));
    let check = node(&graph, "goal_check");
    assert_eq!(
        target_name(&graph, node(&graph, "verify").routing.groups[0].arms[0].to),
        "goal_check"
    );
    let arms = &check.routing.groups[0].arms;
    assert_eq!(arms.len(), 3);
    // Gates in id order: `verify` falls to the graph target, `work` to its own
    // fallback (its `retry_target` names a node that does not exist).
    assert_eq!(target_name(&graph, arms[0].to), "plan");
    assert!(arms[0].back);
    assert_eq!(target_name(&graph, arms[1].to), "work");
    assert!(arms[1].back);
    assert_eq!(target_name(&graph, arms[2].to), "exit");
    assert!(!arms[2].back);
    // An unvisited gate fails the check; success-like records pass it.
    let none = statics("success", &json!({}));
    assert!(
        eval_guard(&graph, arms[0].guard, &none, &[]),
        "unvisited: jump"
    );
    assert!(
        !eval_guard(&graph, arms[2].guard, &none, &[]),
        "unvisited: no exit"
    );
    assert!(check.budget.is_finite());
    assert_eq!(node(&graph, "plan").budget.max_firings, MAX_FIRINGS);
    assert_eq!(node(&graph, "plan").join, JoinPolicy::Any);
    ir::validate(&graph).expect("validates");
}

#[test]
fn loops_get_back_edges_any_joins_and_capped_budgets() {
    let graph = lower_ok(&dot(r#"
        graph [max_node_visits=30]
        impl [prompt="x", max_visits=12]
        check [shape=diamond]
        fix [prompt="x", max_visits=5]
        start -> impl -> check
        check -> exit [condition="outcome=succeeded"]
        check -> impl [label="Retry"]
        impl -> fix
        fix -> check [loop_restart=true]
    "#));
    let retry = &node(&graph, "check").routing.groups[0].arms[1];
    assert!(retry.back, "the cycle-closing edge is a back edge");
    assert_eq!(node(&graph, "impl").join, JoinPolicy::Any);
    assert_eq!(node(&graph, "impl").budget.max_firings, 12);
    assert_eq!(node(&graph, "check").budget.max_firings, 30);
    assert_eq!(node(&graph, "fix").budget.max_firings, 5);
    assert_eq!(
        node(&graph, "fix").routing.groups[0].arms[0].transition,
        EdgeTransition::Restart
    );
    ir::validate(&graph).expect("validates");
}

#[test]
fn visit_limits_above_the_cap_are_rejected_and_unlimited_is_capped_with_a_note() {
    assert!(
        codes(&dot(r#"
        a [prompt="x", max_visits=501]
        start -> a -> exit
    "#))
        .contains(&"fabro.max_visits_too_large".to_string())
    );
    let lowered = frontend_fabro::load_text(
        "w.fabro",
        &dot(r#"
        a [prompt="x"]
        start -> a
        a -> a [condition="outcome=failed"]
        a -> exit
    "#),
    );
    let graph = lowered.graph.expect("lowers");
    assert_eq!(node(&graph, "a").budget.max_firings, MAX_FIRINGS);
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "info.budget.default")
    );
}

#[test]
fn static_fan_out_and_fan_in_lower_to_groups_and_an_all_join() {
    let graph = lower_ok(&dot(r#"
        fork [shape=component]
        a [prompt="x"]
        b [prompt="x"]
        merge [shape=tripleoctagon]
        report [prompt="x"]
        start -> fork
        fork -> a
        fork -> b
        a -> merge
        b -> merge
        merge -> report -> exit
    "#));
    assert_eq!(
        node(&graph, "fork").routing.groups.len(),
        2,
        "one group per branch"
    );
    assert_eq!(node(&graph, "merge").join, JoinPolicy::All);
    assert_eq!(node(&graph, "a").join, JoinPolicy::Any);
    let arm = &node(&graph, "a").routing.groups[0].arms[0];
    assert!(arm.map.is_some(), "a branch hands the fan-in its result");
    assert!(contains_placeholder(&node(&graph, "merge").step.config));
    ir::validate(&graph).expect("validates");
}

#[test]
fn for_each_lowers_to_an_expansion_on_the_template_node() {
    let graph = lower_ok(&dot(r#"
        plan [shape=parallelogram, script="echo"]
        fan [shape=component, for_each="context.jobs", max_parallel=4]
        job [prompt="x"]
        join [shape=tripleoctagon]
        start -> plan -> fan -> job -> join -> exit
    "#));
    let job = node(&graph, "job");
    let Some(ir::Expansion::ForEach {
        max_parallel,
        fail_fast,
        target,
        ..
    }) = &job.expand
    else {
        panic!("job expands");
    };
    assert_eq!(*max_parallel, Some(4));
    assert!(!fail_fast);
    assert_eq!(*target, ir::ExpandTarget::Node);
    assert!(
        node(&graph, "fan").precondition.is_some(),
        "the item cap is a precondition"
    );
    assert!(contains_placeholder(&node(&graph, "fan").step.config));
    assert!(
        codes(&dot(r#"
        fan [shape=component, for_each="context.jobs"]
        c [shape=parallelogram, script="x"]
        start -> fan -> c -> exit
    "#))
        .contains(&"fabro.for_each.target".to_string())
    );
}

#[test]
fn human_gates_offer_their_edges_as_choices() {
    let graph = lower_ok(&dot(r#"
        gate [shape=hexagon, label="Approve?", question_type="multiple_choice"]
        yes [prompt="x"]
        no [prompt="x"]
        free [prompt="x"]
        start -> gate
        gate -> yes [label="[A] Approve"]
        gate -> no [label="Reject"]
        gate -> free [freeform=true]
        yes -> exit
        no -> exit
        free -> exit
    "#));
    let config = &node(&graph, "gate").step.config;
    assert_eq!(
        config["choices"],
        json!([
            { "key": "A", "label": "[A] Approve", "to": "yes" },
            { "key": "R", "label": "Reject", "to": "no" },
        ])
    );
    assert_eq!(config["freeform_target"], json!("free"));
    assert_eq!(config["question_type"], json!("multiple_choice"));
    // The labelled tier matches an accelerator-free answer.
    let t = tiers(&graph, "gate");
    let answer = statics("success", &json!({ "preferred_label": "Approve" }));
    assert!(eval_guard(&graph, t[0].1[0].1, &answer, &[]));
    assert!(!eval_guard(&graph, t[0].1[1].1, &answer, &[]));
}

#[test]
fn stylesheets_write_model_properties_that_explicit_attributes_beat() {
    let graph = lower_ok(&dot(r#"
        graph [model_stylesheet="* { model: a; } .code { model: b; reasoning_effort: high } #c { model: c }"]
        x [prompt="x"]
        y [prompt="x", class="code"]
        c [prompt="x", class="code", model="mine"]
        start -> x -> y -> c -> exit
    "#));
    assert_eq!(node(&graph, "x").step.config["model"], json!("a"));
    assert_eq!(node(&graph, "y").step.config["model"], json!("b"));
    assert_eq!(
        node(&graph, "y").step.config["reasoning_effort"],
        json!("high")
    );
    assert_eq!(node(&graph, "c").step.config["model"], json!("mine"));
    assert_eq!(node(&graph, "y").meta["model"], json!("b"));
}

#[test]
fn templates_render_inputs_and_unbound_inputs_are_specific_rejections() {
    let inputs = CompileInputs::new().with_input("name", "Ada");
    let graph = lower_ok_with(
        &dot(r#"
        graph [goal="Greet {{ inputs.name }}"]
        a [prompt="Say hi to {{ inputs.name }} for {{ goal }}"]
        c [shape=parallelogram, script="echo {{ inputs.name }} {{ goal }}"]
        start -> a -> c -> exit
    "#),
        &frontend::NoFiles,
        &inputs,
    );
    assert_eq!(
        node(&graph, "a").step.config["prompt"],
        json!("Say hi to Ada for Greet Ada")
    );
    assert_eq!(
        node(&graph, "c").step.config["script"],
        json!("echo Ada 'Greet Ada'")
    );
    assert_eq!(graph.params["goal"], json!("Greet Ada"));
    assert_eq!(graph.params["inputs"], json!({ "name": "Ada" }));
    assert!(
        codes(&dot(r#"
        a [prompt="{{ inputs.missing }}"]
        start -> a -> exit
    "#))
        .contains(&"unsupported.template.unbound_input".to_string())
    );
}

#[test]
fn file_references_and_workflow_toml_defaults_resolve_through_the_file_source() {
    let files = files(&[
        (
            "wf/prompts/plan.md",
            "Plan {{ inputs.mode }}\n{% include \"partials/tail.md\" %}",
        ),
        ("wf/prompts/partials/tail.md", "tail"),
        ("wf/workflow.toml", "[run.inputs]\nmode = \"fast\"\n"),
    ]);
    let text = dot(r#"
        a [prompt="@prompts/plan.md"]
        start -> a -> exit
    "#);
    let lowered = frontend_fabro::load("wf/workflow.fabro", &text, &files, &CompileInputs::new());
    let graph = lowered.graph.expect("lowers");
    assert_eq!(
        node(&graph, "a").step.config["prompt"],
        json!("Plan fast\ntail")
    );
    assert!(
        codes(&dot(r#"
        a [prompt="@missing.md"]
        start -> a -> exit
    "#))
        .contains(&"fabro.file_not_found".to_string())
    );
}

#[test]
fn structural_mistakes_are_specific_errors() {
    assert!(
        codes("digraph G { a [prompt=\"x\"] a -> exit  exit [shape=Msquare] }")
            .contains(&"fabro.no_start".to_string())
    );
    assert!(codes(&dot("start -> ghost")).contains(&"fabro.undeclared_node".to_string()));
    assert!(
        codes(&dot(r#"
        a [prompt="x"]
        b [prompt="x"]
        start -> a -> exit
    "#))
        .contains(&"fabro.unreachable_node".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x"]
        start -> a -> exit
        a -> start
    "#))
        .contains(&"fabro.start_has_incoming".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x"]
        start -> a -> exit -> a
    "#))
        .contains(&"fabro.exit_has_outgoing".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [llm_prompt="x"]
        start -> a -> exit
    "#))
        .contains(&"unsupported.attractor".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", timeout=1200]
        start -> a -> exit
    "#))
        .contains(&"unsupported.attractor".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", import="other.fabro"]
        start -> a -> exit
    "#))
        .contains(&"unsupported.import".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", frobnicate=1]
        start -> a -> exit
    "#))
        .contains(&"fabro.unknown_attribute".to_string())
    );
    assert!(
        codes(&dot(r#"
        a [prompt="x", color=red, timeout="10s"]
        start -> a -> exit
    "#))
        .is_empty(),
        "layout attributes are dropped silently"
    );
}

#[test]
fn timeouts_lower_to_per_attempt_budgets_with_fabro_defaults() {
    let graph = lower_ok(&dot(r#"
        a [prompt="x", timeout="20m"]
        c [shape=parallelogram, script="true"]
        start -> a -> c -> exit
    "#));
    assert_eq!(node(&graph, "a").budget.timeout, Duration::from_secs(1200));
    assert_eq!(
        node(&graph, "a").step.config["timeout_ms"],
        json!(1_200_000)
    );
    assert_eq!(node(&graph, "c").budget.timeout, Duration::from_secs(600));
}

#[test]
fn stdin_source_reads_the_context_or_the_fan_in() {
    let graph = lower_ok(&dot(r#"
        fork [shape=component]
        a [prompt="x"]
        merge [shape=tripleoctagon]
        m [shape=parallelogram, script="cat", stdin_source="context.parallel.results"]
        k [shape=parallelogram, script="cat", stdin_source="context.output.a"]
        start -> fork -> a -> merge -> m -> k -> exit
    "#));
    let stdin = |name: &str| {
        let id = node(&graph, name).step.config["stdin"]["$expr"]
            .as_u64()
            .expect("placeholder");
        print_expr(
            &graph.exprs,
            ir::ExprId::new(u32::try_from(id).expect("u32")),
        )
    };
    assert_eq!(stdin("m"), "nodes.merge.output");
    assert_eq!(stdin("k"), "get(kv, 'output.a')");
}
