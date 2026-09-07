//! Acceptance §7 item 3, the pure half: what lowering produces — routing
//! tiers, failure policies, goal gates, parallel, budgets, and the specific
//! rejections. The run half is `crates/fabro/acceptance/tests/routing.rs`.

mod support;

use std::time::Duration;
use std::{env, fs, process};

use frontend::print::print_expr;
use frontend::{CompileInputs, Frontend};
use frontend_fabro::kinds::{
    AGENT_KIND, COMMAND_KIND, HUMAN_KIND, PROMPT_KIND, WAIT_KIND, WORKFLOW_KIND,
};
use frontend_fabro::{Fabro, MAX_FIRINGS};
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
    assert_eq!(node(&graph, "start").step.kind, STAGE_KIND);
    assert_eq!(node(&graph, "start").step.config["kind"], json!("start"));
    assert_eq!(node(&graph, "exit").step.kind, STAGE_KIND);
    assert_eq!(node(&graph, "d").step.kind.as_str(), "noop");
    assert_eq!(node(&graph, "a").step.kind, AGENT_KIND);
    assert_eq!(node(&graph, "p").step.kind, PROMPT_KIND);
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

/// A promoting policy hands the step the node's explicit routes, so the step
/// can keep a failure an explicit route matches, as Fabro's executor does.
#[test]
fn promoting_policies_carry_the_explicit_routes() {
    let graph = lower_ok(&dot(r#"
        a [prompt="x", on_failure="succeed"]
        b [prompt="x"]
        c [prompt="x"]
        d [prompt="x", on_failure="route"]
        start -> a
        a -> b [condition="outcome=failed"]
        a -> c [label="[C] Continue"]
        a -> exit
        b -> exit
        c -> d -> exit
    "#));
    let routes = &node(&graph, "a").step.config["routes"];
    assert_eq!(routes["conditions"], json!(["outcome=failed"]));
    assert_eq!(routes["labels"], json!(["continue"]));
    assert_eq!(routes["targets"], json!(["c", "exit"]));
    assert!(
        node(&graph, "d").step.config.get("routes").is_none(),
        "a routing policy needs no promotion check"
    );
    let found = codes(&dot(r#"
        a [prompt="x", on_failure="partially_succeed"]
        start -> a -> exit
    "#));
    assert!(
        found.contains(&"fabro.petri_extension".to_string()),
        "the Petri-only spelling is named: {found:?}"
    );
}

#[test]
fn succeed_is_supported_and_auto_status_is_a_warned_alias() {
    let text = dot(r#"
        a [prompt="x", on_failure="succeed"]
        b [prompt="x", auto_status=true]
        start -> a -> b -> exit
    "#);
    let found = codes(&text);
    assert!(
        !found.iter().any(|code| code.contains("succeed")),
        "`on_failure=\"succeed\"` is supported without a warning: {found:?}"
    );
    assert!(
        found.contains(&"deprecated.auto_status".to_string()),
        "{found:?}"
    );
    let graph = lower_ok(&text);
    assert_eq!(
        node(&graph, "a").step.config["on_failure"],
        json!("succeed")
    );
    assert_eq!(
        node(&graph, "b").step.config["on_failure"],
        json!("succeed"),
        "`auto_status=true` is the node's `on_failure=\"succeed\"`"
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
fn static_fan_in_payloads_keep_the_fan_out_branch_order() {
    let graph = lower_ok(&dot(r#"
        fork [shape=component]
        a [prompt="x"]
        a_tail [prompt="x"]
        b [prompt="x"]
        merge [shape=tripleoctagon]
        start -> fork
        fork -> a -> a_tail -> merge
        fork -> b -> merge
        merge -> exit
    "#));
    let index = |name: &str| {
        let map = node(&graph, name).routing.groups[0].arms[0]
            .map
            .expect("branch payload");
        eval_expr(&graph, map, &statics("success", &json!(name)), &[])["index"]
            .as_u64()
            .expect("numeric branch index")
    };
    assert_eq!(index("a_tail"), 0);
    assert_eq!(index("b"), 1);
}

#[test]
fn parallel_edges_to_exit_still_pass_through_the_goal_check() {
    let graph = lower_ok(&dot(r#"
        fork [shape=component]
        gate [prompt="x", goal_gate=true, retry_target="gate"]
        start -> fork
        fork -> gate
        fork -> exit
        gate -> exit
    "#));
    let targets: Vec<String> = node(&graph, "fork")
        .routing
        .groups
        .iter()
        .flat_map(|group| group.arms.iter())
        .map(|arm| target_name(&graph, arm.to))
        .collect();
    assert_eq!(targets, ["gate", "goal_check"]);
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
        (
            "wf/prompts/partials/tail.md",
            "tail {% include \"nested/end.md\" %}",
        ),
        ("wf/prompts/partials/nested/end.md", "done"),
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
        json!("Plan fast\ntail done")
    );
    assert!(
        codes(&dot(r#"
        a [prompt="@missing.md"]
        start -> a -> exit
    "#))
        .contains(&"fabro.file_not_found".to_string())
    );
}

/// Every `workflow.toml` section is diagnosed. Platform-only sections warn
/// with why, a requirement the standalone runner cannot meet is an
/// `unsupported.workflow_toml.*` error, and a key Fabro's own parser refuses
/// is an error with Fabro's rename hint.
#[test]
fn workflow_toml_sections_warn_or_reject_and_never_pass_silently() {
    let diags = |toml: &str| {
        let files = files(&[("wf/workflow.toml", toml)]);
        frontend_fabro::load(
            "wf/workflow.fabro",
            &dot(r#"
                a [shape=parallelogram, script="true"]
                start -> a -> exit
            "#),
            &files,
            &CompileInputs::new(),
        )
        .diagnostics
        .iter()
        .cloned()
        .collect::<Vec<_>>()
    };
    let codes = |toml: &str| {
        let mut out: Vec<String> = diags(toml).iter().map(|d| d.code.to_string()).collect();
        out.sort();
        out.dedup();
        out
    };

    // The code-review bundle's platform-only sections: warnings, and the
    // graph still lowers.
    let platform_only = diags(
        "_version = 1\n[workflow]\ngraph = \"workflow.fabro\"\n[run]\ngoal = \"g\"\n\
         [run.inputs]\nmode = \"changes\"\n[run.clone]\ndepth = 1\n[run.run_branch]\n\
         enabled = false\n[run.pull_request]\nenabled = false\n[run.model.fallbacks]\n\
         \"m\" = [\"p:m\"]\n[run.model]\nprovider = \"openai\"\n[run.environment]\n\
         id = \"review\"\n[run.integrations.github.permissions]\npull_requests = \"write\"\n\
         [run.checkpoint]\nexclude_globs = []\n[run.artifacts]\ninclude = []\n\
         [run.execution]\nmode = \"normal\"\n[run.agent]\nfabro_tools = true\n\
         [environments.review]\nprovider = \"docker\"\n[environments.review.network]\n\
         mode = \"none\"\n[environments.review.image]\ndockerfile = { path = \"Dockerfile\" }\n",
    );
    assert!(
        platform_only.iter().all(|d| !d.is_error()),
        "platform-only sections are warnings: {platform_only:#?}"
    );
    let platform_codes: Vec<String> = platform_only.iter().map(|d| d.code.to_string()).collect();
    for code in [
        "ignored.workflow_toml.run.clone",
        "ignored.workflow_toml.run.run_branch",
        "ignored.workflow_toml.run.pull_request",
        "ignored.workflow_toml.run.model.fallbacks",
        "ignored.workflow_toml.run.integrations",
        "ignored.workflow_toml.run.checkpoint",
        "ignored.workflow_toml.run.artifacts",
        "ignored.workflow_toml.run.agent.fabro_tools",
        "ignored.workflow_toml.environments.review.network",
        "ignored.workflow_toml.environments.review.image.dockerfile",
    ] {
        assert!(
            platform_codes.contains(&code.to_string()),
            "{code} in {platform_codes:?}"
        );
    }
    // Sections the runner now applies do not warn.
    for code in [
        "ignored.workflow_toml.run.goal",
        "ignored.workflow_toml.run.model",
        "ignored.workflow_toml.run.environment",
        "ignored.workflow_toml.run.execution",
        "ignored.workflow_toml.environments",
    ] {
        assert!(
            !platform_codes.contains(&code.to_string()),
            "{code} is applied, not ignored: {platform_codes:?}"
        );
    }
    assert!(
        platform_only
            .iter()
            .all(|d| d.message.contains("is ignored: ")),
        "each warning says why: {platform_only:#?}"
    );

    // Requirements the standalone runner cannot meet: specific errors.
    assert_eq!(
        codes("[run.environment]\nid = \"nowhere\"\n"),
        ["unsupported.workflow_toml.run.environment"],
        "an environment id with no table is refused, as Fabro refuses it"
    );
    assert_eq!(
        codes("[run.environment]\nid = \"e\"\n[environments.e]\nprovider = \"k8s\"\n"),
        ["unsupported.workflow_toml.environments.provider"]
    );
    assert_eq!(
        codes("[run.prepare]\nsteps = [{ script = \"a\", command = [\"b\"] }]\n"),
        ["unsupported.workflow_toml.run.prepare"],
        "exactly one of script or command"
    );
    // A hook whose event Fabro does not know is a specific error; a good one
    // loads (see `hooks_load_from_every_layer_and_merge_by_id`).
    assert_eq!(
        codes("[[run.hooks]]\nevent = \"stage.completed\"\nscript = \"true\"\n"),
        ["fabro.hooks.event"]
    );
    assert_eq!(
        codes("[run.agent.mcps.files]\ntype = \"stdio\"\ncommand = \"mcp\"\n"),
        ["unsupported.workflow_toml.run.agent.mcps"]
    );
    // An empty hook list or MCP table asks for nothing.
    assert!(codes("[run]\nhooks = []\n[run.agent.mcps]\n").is_empty());

    // Keys Fabro's parser refuses, with its rename hint.
    let legacy = diags("version = 1\n[vars]\nmode = \"x\"\n[llm]\nmodel = \"m\"\n");
    let hints: Vec<(String, Option<String>)> = legacy
        .iter()
        .filter(|d| d.is_error())
        .map(|d| (d.code.to_string(), d.hint.clone()))
        .collect();
    assert!(
        hints.contains(&(
            "unsupported.workflow_toml.key".to_string(),
            Some("rename to `_version`".to_string())
        )),
        "{hints:?}"
    );
    assert!(
        hints.contains(&(
            "unsupported.workflow_toml.key".to_string(),
            Some("rename to `[run.inputs]`".to_string())
        )),
        "{hints:?}"
    );
    assert!(
        hints.contains(&(
            "unsupported.workflow_toml.key".to_string(),
            Some("rename to `[run.model]`".to_string())
        )),
        "{hints:?}"
    );
    assert_eq!(codes("_version = 2\n"), [
        "unsupported.workflow_toml.version"
    ]);
    assert_eq!(codes("[run]\nnot_a_key = 1\n"), [
        "unsupported.workflow_toml.key"
    ]);
    // A clean file is clean.
    assert!(codes("_version = 1\n[workflow]\ngraph = \"workflow.fabro\"\n").is_empty());
}

/// Imports expand at load as Fabro's transform expands them: prefixed ids,
/// dropped sentinels, spliced boundary edges, inherited defaults, propagated
/// classes, rewritten retry targets, nested imports relative to their own
/// file, and Fabro's refusals with Fabro's messages.
#[test]
fn imports_expand_at_load_with_fabro_rules() {
    let files = files(&[
        (
            "flows/checks.fabro",
            r#"digraph Checks {
                start [shape=Mdiamond]
                exit [shape=Msquare]
                lint [shape=parallelogram, script="lint", retry_target="lint"]
                test [prompt="@prompts/test.md", class="verification"]
                start -> lint -> test -> exit
            }"#,
        ),
        (
            "flows/prompts/test.md",
            "Run the tests for {{ inputs.target }}.",
        ),
        (
            "flows/outer.fabro",
            r#"digraph Outer {
                start [shape=Mdiamond]
                exit [shape=Msquare]
                inner [import="checks.fabro", model="m1"]
                start -> inner -> exit
            }"#,
        ),
        (
            "flows/loop.fabro",
            r#"digraph Loop {
                start [shape=Mdiamond]
                exit [shape=Msquare]
                again [import="loop.fabro"]
                start -> again -> exit
            }"#,
        ),
        (
            "flows/bad.fabro",
            r#"digraph Bad {
                start [shape=Mdiamond]
                exit [shape=Msquare]
                a [prompt="x"]
                b [prompt="x"]
                start -> a
                start -> b
                a -> exit
                b -> exit
            }"#,
        ),
        (
            "flows/empty.fabro",
            "digraph Empty {\n                start [shape=Mdiamond]\n                exit \
             [shape=Msquare]\n                start -> exit\n            }",
        ),
    ]);
    let inputs = CompileInputs::new().with_input("target", "main");
    let load = |text: &str| frontend_fabro::load("flows/main.fabro", text, &files, &inputs);
    let lowered = load(&dot(r#"
        build [shape=parallelogram, script="build"]
        review [import="checks.fabro", model="m2", reasoning_effort="high", class="Review Step"]
        ship [shape=parallelogram, script="ship"]
        start -> build
        build -> review [label="[G] Go"]
        review -> ship [condition="outcome=succeeded"]
        review -> exit
        ship -> exit
    "#));
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    let graph = lowered.graph.expect("lowers");
    let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"review.lint") && names.contains(&"review.test"));
    assert!(
        !names.contains(&"review"),
        "the placeholder is gone: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("start") && n != &"start"),
        "the import's sentinels are dropped: {names:?}"
    );
    let test = node(&graph, "review.test");
    assert_eq!(test.step.config["model"], json!("m2"), "inherited default");
    assert_eq!(test.step.config["reasoning_effort"], json!("high"));
    assert_eq!(
        test.step.config["prompt"],
        json!("Run the tests for main."),
        "an @file inside the import resolves beside the imported file"
    );
    assert_eq!(
        test.meta["classes"],
        json!(["Review", "Step", "verification", "review"]),
        "the placeholder's classes as parsed, the import's own, then Fabro's class from the \
         placeholder id"
    );
    let lint = node(&graph, "review.lint");
    assert_eq!(lint.step.kind, COMMAND_KIND);
    assert!(
        lint.step.config.get("model").is_none(),
        "a command node takes no model default"
    );
    // The boundary edges keep their attributes and reach the right ends.
    let build_tiers = tiers(&graph, "build");
    assert_eq!(
        build_tiers[0].1[0].0, "review.lint",
        "incoming edge to the entry"
    );
    let review_tiers = tiers(&graph, "review.test");
    assert_eq!(
        review_tiers[0].1[0].0, "ship",
        "outgoing edge from the exit predecessor"
    );
    // Retry targets inside the import are rewritten to the prefix.
    let goal = load(&dot(r#"
        review [import="checks.fabro"]
        start -> review -> exit
    "#));
    assert!(!goal.diagnostics.has_errors(), "{:?}", goal.diagnostics);

    // Nested imports and their cycle.
    let nested = load(&dot(r#"
        outer [import="outer.fabro"]
        start -> outer -> exit
    "#));
    assert!(!nested.diagnostics.has_errors(), "{:?}", nested.diagnostics);
    let graph = nested.graph.expect("lowers");
    assert_eq!(
        node(&graph, "outer.inner.test").step.config["model"],
        json!("m1"),
        "the inner placeholder's default reaches the doubly imported node"
    );
    let cycle = load(&dot(r#"
        again [import="loop.fabro"]
        start -> again -> exit
    "#));
    let messages: Vec<String> = cycle
        .diagnostics
        .iter()
        .filter(|d| d.code == "fabro.import")
        .map(|d| d.message.clone())
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("circular import detected")),
        "{messages:?}"
    );

    // Fabro's refusals, with Fabro's reasons.
    let refused = |body: &str| -> Vec<String> {
        load(&dot(body))
            .diagnostics
            .iter()
            .filter(|d| d.code == "fabro.import")
            .map(|d| d.message.clone())
            .collect()
    };
    assert!(
        refused("bad [import=\"bad.fabro\"]\nstart -> bad -> exit")
            .iter()
            .any(|m| m.contains("must have exactly one successor")),
    );
    assert!(
        refused("x [import=\"checks.fabro\", prompt=\"no\"]\nstart -> x -> exit")
            .iter()
            .any(|m| m.contains("has unsupported attribute 'prompt'")),
    );
    assert!(
        refused("x [import=\"missing.fabro\"]\nstart -> x -> exit")
            .iter()
            .any(|m| m.contains("file not found")),
    );
    assert!(
        refused(
            "x [import=\"empty.fabro\"]\ny [prompt=\"p\"]\nstart -> x [label=\"[A] A\"]\nx -> y\ny -> exit"
        )
        .iter()
        .any(|m| m.contains("cannot bypass semantic edges")),
    );
    // An empty import with plain edges is removed and its neighbours wired.
    let bypass = load(&dot(r#"
        x [import="empty.fabro"]
        y [prompt="p"]
        start -> x -> y -> exit
    "#));
    assert!(!bypass.diagnostics.has_errors(), "{:?}", bypass.diagnostics);
    let graph = bypass.graph.expect("lowers");
    assert_eq!(tiers(&graph, "start")[0].1[0].0, "y");
}

/// `[run.environment]` maps the provider onto the launch settings the CLI
/// reads, the image onto the scope's container target, and literal env onto
/// the scope env; `[run.prepare]` steps become the first command nodes.
#[test]
fn run_environment_and_prepare_lower_onto_the_scope_and_the_graph() {
    let files = files(&[(
        "wf/workflow.toml",
        "[run]\ngoal = \"Fix {{ inputs.target }}\"\n[run.inputs]\ntarget = \"main\"\n\
         [run.model]\nprovider = \"openai\"\nname = \"gpt-5.6-sol\"\n\
         [run.model.controls]\nreasoning_effort = \"low\"\n\
         [run.execution]\nmode = \"dry_run\"\napproval = \"auto\"\n\
         [run.environment]\nid = \"review\"\n[run.environment.env]\nOVERRIDE = \"run\"\n\
         [environments.review]\nprovider = \"docker\"\n[environments.review.image]\n\
         docker = \"ghcr.io/acme/review:1\"\n[environments.review.resources]\ncpu = 4\n\
         [environments.review.env]\nLANG = \"C.UTF-8\"\nOVERRIDE = \"base\"\n\
         TOKEN = \"{{ secrets.REVIEW_TOKEN }}\"\n\
         [run.prepare]\ntimeout = \"30s\"\n[[run.prepare.steps]]\nscript = \"make deps\"\n\
         [[run.prepare.steps]]\ncommand = [\"sh\", \"-c\", \"echo {{ inputs.target }}\"]\n\
         env = { STEP = \"two\" }\n",
    )]);
    let lowered = frontend_fabro::load(
        "wf/workflow.fabro",
        &dot(r#"
            a [prompt="x"]
            c [shape=parallelogram, script="true"]
            start -> a -> c -> exit
        "#),
        &files,
        &CompileInputs::new(),
    );
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    let graph = lowered.graph.expect("lowers");
    // Launch settings, as the CLI reads them.
    let launch = Fabro::new().launch_settings(&graph);
    assert_eq!(launch.sandbox_backend.as_deref(), Some("docker"));
    assert!(launch.dry_run && launch.auto_approve);
    assert_eq!(
        graph.params["fabro.launch"]["cpu_cores"],
        json!(null),
        "resources size Daytona only"
    );
    assert_eq!(
        graph.params["goal"],
        json!("Fix main"),
        "[run] goal renders and applies"
    );
    // The scope: image and literal env; the secret is not on the scope.
    let scope = &graph.scopes[0];
    assert!(
        matches!(&scope.runtime.target, ir::RuntimeTarget::Container { image, .. } if image == "ghcr.io/acme/review:1"),
        "{:?}",
        scope.runtime.target
    );
    assert_eq!(scope.env["LANG"], ir::ExprOrValue::Value(json!("C.UTF-8")));
    assert_eq!(
        scope.env["OVERRIDE"],
        ir::ExprOrValue::Value(json!("run")),
        "the run's env wins over the named environment's"
    );
    assert!(
        !scope.env.contains_key("TOKEN"),
        "a secret never lands on the scope"
    );
    // Commands carry the secret as a reference, resolved at spawn.
    assert_eq!(
        node(&graph, "c").step.config["env"]["TOKEN"],
        json!({ "$secret": "REVIEW_TOKEN" })
    );
    // Model defaults reach the LLM node.
    let a = &node(&graph, "a").step.config;
    assert_eq!(a["model"], json!("gpt-5.6-sol"));
    assert_eq!(a["provider"], json!("openai"));
    assert_eq!(a["reasoning_effort"], json!("low"));
    // Prepare steps: first after start, in order, with env, timeout, exit.
    assert_eq!(tiers(&graph, "start")[0].1[0].0, "run_prepare_1");
    assert_eq!(tiers(&graph, "run_prepare_1")[0].1[0].0, "run_prepare_2");
    assert_eq!(tiers(&graph, "run_prepare_2")[0].1[0].0, "a");
    let two = node(&graph, "run_prepare_2");
    assert_eq!(two.step.kind, COMMAND_KIND);
    assert_eq!(two.step.config["script"], json!("sh -c 'echo main'"));
    assert_eq!(two.step.config["env"]["STEP"], json!("two"));
    assert_eq!(
        two.step.config["env"]["TOKEN"],
        json!({ "$secret": "REVIEW_TOKEN" })
    );
    assert_eq!(two.step.config["on_failure"], json!("exit"));
    assert_eq!(two.budget.timeout, Duration::from_secs(30));
    assert_eq!(two.meta["classes"], json!(["run-prepare"]));
    // A reserved id is refused.
    let clash = frontend_fabro::load(
        "wf/workflow.fabro",
        &dot(r#"
            run_prepare_1 [prompt="x"]
            start -> run_prepare_1 -> exit
        "#),
        &files,
        &CompileInputs::new(),
    );
    assert!(
        clash
            .diagnostics
            .iter()
            .any(|d| d.code == "fabro.reserved_node_id"),
        "{:?}",
        clash.diagnostics
    );
}

#[test]
fn structural_mistakes_are_specific_errors() {
    let id_only = lower_ok("digraph G { start; exit; start -> exit }");
    assert_eq!(id_only.entry, [node_id(&id_only, "start")]);
    assert_eq!(
        id_only.completion,
        Completion::TerminalNode(node_id(&id_only, "exit"))
    );
    let typed = lower_ok("digraph G { begin [type=start]; done [type=exit]; begin -> done }");
    assert_eq!(typed.entry, [node_id(&typed, "begin")]);
    assert_eq!(
        typed.completion,
        Completion::TerminalNode(node_id(&typed, "done"))
    );
    assert!(
        codes("digraph G { start; other [shape=Mdiamond]; exit; start -> other -> exit }")
            .contains(&"fabro.multiple_starts".to_string())
    );
    assert!(
        codes("digraph G { start; exit; done [shape=Msquare]; start -> exit; start -> done }")
            .contains(&"fabro.multiple_exits".to_string())
    );
    assert!(
        codes(&dot(
            "goal_check [prompt=\"x\"] start -> goal_check -> exit"
        ))
        .contains(&"fabro.reserved_node_id".to_string())
    );
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
        .contains(&"fabro.import".to_string()),
        "a missing import file is an import error"
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
fn unused_attributes_warn_and_unbounded_agent_repairs_are_rejected() {
    let diags = diagnostics(&dot(r#"
        a [prompt="x", review_target=true, stall_timeout="1m"]
        start -> a -> exit
    "#));
    for code in ["ignored.review_target", "fabro.unknown_attribute"] {
        assert!(
            diags.iter().any(|diagnostic| diagnostic.code == code),
            "{code} in {diags:?}"
        );
    }
    assert!(
        codes(&dot(r#"
            a [prompt="x", output_retries=101]
            start -> a -> exit
        "#))
        .contains(&"fabro.output_retries_too_large".to_string())
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

#[test]
fn negative_weights_shift_so_the_lowest_is_zero_and_order_holds() {
    let graph = lower_ok(&dot(r#"
        a [prompt="x"]
        b [prompt="x"]
        c [prompt="x"]
        d [prompt="x"]
        start -> a
        a -> b [weight=-1]
        a -> c [weight=-5]
        a -> d
        a -> exit [weight=2]
        b -> exit
        c -> exit
        d -> exit
    "#));
    let weights: Vec<u32> = node(&graph, "a").routing.groups[0]
        .arms
        .iter()
        .map(|a| a.weight)
        .collect();
    assert_eq!(weights, [4, 0, 5, 7], "shifted by the lowest, -5");
    let graph = lower_ok(&dot(r#"
        graph [selection="random"]
        a [prompt="x"]
        b [prompt="x"]
        start -> a
        a -> b [weight=-3]
        a -> exit [weight=2]
        b -> exit
    "#));
    let weights: Vec<u32> = node(&graph, "a").routing.groups[0]
        .arms
        .iter()
        .map(|a| a.weight)
        .collect();
    assert_eq!(
        weights,
        [1, 2],
        "random: a weight at or below zero counts as one"
    );
}

/// REMOVE AFTER 2026-10-04 with the shim.
#[test]
fn a_succeed_node_reads_its_converted_failure_as_succeeded() {
    let graph = lower_ok(&dot(r#"
        a [prompt="x", on_failure="succeed"]
        b [prompt="x"]
        c [prompt="x"]
        d [prompt="x"]
        start -> a
        a -> b [condition="outcome=succeeded"]
        a -> c [condition="outcome=partially_succeeded"]
        a -> d [condition="outcome=failed"]
        a -> exit
        b -> exit
        c -> exit
        d -> exit
    "#));
    let tiers = tiers(&graph, "a");
    let (succeeded, partial, failed) = (tiers[0].1[0].1, tiers[0].1[1].1, tiers[0].1[2].1);
    let converted = statics("partial_success", &json!({ "outcome": "succeeded" }));
    assert!(eval_guard(&graph, succeeded, &converted, &[]));
    assert!(!eval_guard(&graph, partial, &converted, &[]));
    assert!(!eval_guard(&graph, failed, &converted, &[]));
    let exhausted = statics("partial_success", &json!({ "outcome": "failed" }));
    assert!(
        eval_guard(&graph, succeeded, &exhausted, &[]),
        "retries that ran out under `succeed` read as succeeded too"
    );
    let genuine = statics(
        "partial_success",
        &json!({ "outcome": "partially_succeeded" }),
    );
    assert!(!eval_guard(&graph, succeeded, &genuine, &[]));
    assert!(eval_guard(&graph, partial, &genuine, &[]));
    let clean = statics("success", &json!({ "outcome": "succeeded" }));
    assert!(eval_guard(&graph, succeeded, &clean, &[]));
    assert!(matches!(
        tiers.last().expect("fallback").1[0].1,
        Guard::Always
    ));
}

#[test]
fn a_check_with_no_inputs_warns_on_unbound_inputs_and_keeps_the_text() {
    let text = dot(r#"
        graph [goal="Fix {{ inputs.pr }}"]
        a [prompt="Look at {{ inputs.pr }} for {{ vars.owner }}"]
        c [shape=parallelogram, script="gh pr view {{ inputs.pr }}"]
        start -> a -> c -> exit
    "#);
    let lenient = CompileInputs::new().with_unbound_as_warning();
    let lowered = frontend_fabro::load("w.fabro", &text, &frontend::NoFiles, &lenient);
    let graph = lowered.graph.expect("lowers with warnings");
    let found: Vec<&str> = lowered
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    assert!(
        found.iter().all(|c| *c == "fabro.unbound_input"),
        "{found:?}"
    );
    assert_eq!(lowered.diagnostics.errors().count(), 0);
    assert_eq!(
        node(&graph, "a").step.config["prompt"],
        json!("Look at {{ inputs.pr }} for {{ vars.owner }}"),
        "left unrendered"
    );
    assert_eq!(
        node(&graph, "c").step.config["script"],
        json!("gh pr view {{ inputs.pr }}")
    );
    assert!(
        codes(&text).contains(&"unsupported.template.unbound_input".to_string()),
        "strict without the flag"
    );
}

#[test]
fn the_bundle_root_is_the_parent_of_dot_fabro() {
    let base = env::temp_dir().join(format!("petri-fabro-root-{}", process::id()));
    let inside = base.join(".fabro/workflows/one");
    fs::create_dir_all(&inside).expect("create the bundle");
    fs::create_dir_all(base.join("docs")).expect("create docs");
    let frontend = Fabro::new();
    assert_eq!(
        frontend.repo_root(&inside.join("workflow.fabro")),
        base,
        "a file inside the bundle belongs to the bundle's parent"
    );
    assert_eq!(frontend.repo_root(&base.join("docs/demo.fabro")), base);
    let loose = env::temp_dir().join(format!("petri-fabro-loose-{}", process::id()));
    fs::create_dir_all(&loose).expect("create a dir with no bundle");
    assert_eq!(
        frontend.repo_root(&loose.join("w.fabro")),
        loose,
        "no bundle: the file's own directory"
    );
    let _ = fs::remove_dir_all(&base);
    let _ = fs::remove_dir_all(&loose);
}

#[test]
fn a_template_local_set_inside_an_if_renders_and_a_missing_input_is_named() {
    let text = dot(r#"
        graph [model_stylesheet="
            {% if 'kimi' in inputs.model %}{% set effort = 'high' %}{% else %}{% set effort = 'low' %}{% endif %}
            * { model: {{ inputs.model }}; reasoning_effort: {{ effort }}; }
        "]
        a [prompt="x"]
        start -> a -> exit
    "#);
    let graph = lower_ok_with(
        &text,
        &frontend::NoFiles,
        &CompileInputs::new().with_input("model", "kimi-k3"),
    );
    assert_eq!(node(&graph, "a").step.config["model"], json!("kimi-k3"));
    assert_eq!(
        node(&graph, "a").step.config["reasoning_effort"],
        json!("high")
    );
    let strict = diagnostics(&text);
    assert!(
        strict
            .iter()
            .any(|d| d.code == "unsupported.template.unbound_input"
                && d.message.contains("inputs.model")),
        "the missing input is named, not the template-local `effort`: {strict:?}"
    );
    // A lenient check skips the stylesheet it could not render instead of
    // parsing the template text as a stylesheet.
    let lenient = frontend_fabro::load(
        "w.fabro",
        &text,
        &frontend::NoFiles,
        &CompileInputs::new().with_unbound_as_warning(),
    );
    let found: Vec<&str> = lenient
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    assert_eq!(found, ["fabro.unbound_input"], "{found:?}");
    assert!(lenient.graph.is_some());
}

#[test]
fn threads_fidelity_memory_and_controls_lower_onto_agent_nodes_and_edges() {
    let graph = lower_ok(&dot(r#"
        graph [default_fidelity="full", default_thread="shared"]
        plan [prompt="plan", thread_id="impl", class="build"]
        work [prompt="work", fidelity="summary:low", project_memory=false, speed="fast", max_tokens=2000]
        start -> plan
        plan -> work [fidelity="truncate", thread_id="side"]
        work -> exit
    "#));
    let plan = &node(&graph, "plan").step.config;
    assert!(
        plan.get("fidelity").is_none(),
        "the graph default is not the node's own"
    );
    assert_eq!(plan["default_fidelity"], json!("full"));
    assert_eq!(plan["thread_id"], json!("impl"));
    assert_eq!(plan["default_thread"], json!("shared"));
    assert_eq!(plan["classes"], json!(["build"]));
    assert!(plan.get("project_memory").is_none());
    assert!(contains_placeholder(&plan["incoming"]));
    assert!(
        plan["stages"]
            .as_array()
            .is_some_and(|s| s.iter().any(|st| st["id"] == "work"))
    );
    let work = &node(&graph, "work").step.config;
    assert_eq!(work["fidelity"], json!("summary:low"));
    assert_eq!(work["project_memory"], json!(false));
    assert_eq!(work["speed"], json!("fast"));
    assert_eq!(work["max_tokens"], json!(2000));
    // The edge into `work` carries its own fidelity and thread.
    let plan_node = node(&graph, "plan");
    let arm = &plan_node.routing.groups[0].arms[0];
    let map = arm.map.expect("the edge maps its payload");
    let payload = eval_expr(&graph, map, &statics("success", &json!({})), &[]);
    assert_eq!(
        payload,
        json!({"from": "plan", "fidelity": "truncate", "thread_id": "side"})
    );
    assert_eq!(graph.params["fabro_hooks"], json!([]));
}

#[test]
fn thread_ids_without_full_fidelity_warn_and_bad_modes_are_errors() {
    let codes = codes(&dot(r#"
        graph [default_thread="shared"]
        a [prompt="x", thread_id="t"]
        b [prompt="y"]
        start -> a
        a -> b [thread_id="u"]
        b -> exit
    "#));
    assert!(
        codes.contains(&"fabro.thread_id_requires_fidelity_full".to_string()),
        "{codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c.starts_with("ignored.")),
        "{codes:?}"
    );
    let bad = codes_of(&dot(r#"
        a [prompt="x", fidelity="loud", speed="warp"]
        start -> a
        a -> exit [fidelity="quiet"]
    "#));
    assert!(bad.contains(&"fabro.bad_fidelity".to_string()), "{bad:?}");
    assert!(bad.contains(&"fabro.bad_speed".to_string()), "{bad:?}");
    // A thread on a parallel branch is inert, and says so.
    let branch = codes_of(&dot(r#"
        fork [shape=component]
        a [prompt="x", thread_id="t", fidelity="full"]
        join [shape=tripleoctagon]
        start -> fork -> a -> join -> exit
    "#));
    assert!(
        branch.contains(&"fabro.parallel_branch_inert_attribute".to_string()),
        "{branch:?}"
    );
    // `tool_hooks.*` are not Fabro attributes.
    let unknown = diagnostics(&dot(r#"
        a [prompt="x", tool_hooks.pre="echo", tool_hooks.post="echo"]
        start -> a -> exit
    "#));
    let unknown: Vec<&str> = unknown
        .iter()
        .filter(|d| d.code == "fabro.unknown_attribute")
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(unknown.len(), 2, "{unknown:?}");
    assert!(
        unknown.iter().all(|m| m.contains("tool_hooks.")),
        "{unknown:?}"
    );
}

fn codes_of(text: &str) -> Vec<String> {
    codes(text)
}

#[test]
fn hooks_load_from_every_layer_and_merge_by_id() {
    let files = files(&[
        (
            ".fabro/project.toml",
            "[[run.hooks]]\nid = \"guard\"\nevent = \"stage_start\"\nscript = \"project-guard\"\n\
             [[run.hooks]]\nevent = \"run_complete\"\nurl = \"https://example.test/done\"\n",
        ),
        (
            "wf/workflow.toml",
            "[[run.hooks]]\nid = \"guard\"\nevent = \"stage_start\"\nscript = \"workflow-guard\"\nmatcher = \"^agent$\"\n\
             [[run.hooks]]\nevent = \"checkpoint_saved\"\nscript = \"never\"\n",
        ),
    ]);
    let inputs = CompileInputs::new().with_var(
        frontend_fabro::hooks::SETTINGS_HOOKS_VAR,
        "[[run.hooks]]\nevent = \"run_start\"\nscript = \"user-start\"\nsandbox = false\n",
    );
    let lowered = frontend_fabro::load(
        "wf/workflow.fabro",
        &dot(r#"
            a [prompt="x"]
            start -> a -> exit
        "#),
        &files,
        &inputs,
    );
    let codes: Vec<String> = lowered
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect();
    assert!(
        codes.contains(&"fabro.hooks.checkpoint_saved".to_string()),
        "{codes:?}"
    );
    assert!(lowered.diagnostics.errors().count() == 0, "{codes:?}");
    let graph = lowered.graph.expect("lowers");
    let hooks = graph.params["fabro_hooks"]
        .as_array()
        .expect("hook list")
        .clone();
    let summary: Vec<(String, String, String)> = hooks
        .iter()
        .map(|h| {
            (
                h["event"].as_str().unwrap_or("?").to_owned(),
                h["command"]
                    .as_str()
                    .or(h["url"].as_str())
                    .unwrap_or("?")
                    .to_owned(),
                h["source"].as_str().unwrap_or("?").to_owned(),
            )
        })
        .collect();
    assert_eq!(summary, vec![
        (
            "run_start".into(),
            "user-start".into(),
            "settings.toml".into()
        ),
        (
            "stage_start".into(),
            "workflow-guard".into(),
            "wf/workflow.toml".into()
        ),
        (
            "run_complete".into(),
            "https://example.test/done".into(),
            ".fabro/project.toml".into()
        ),
        (
            "checkpoint_saved".into(),
            "never".into(),
            "wf/workflow.toml".into()
        ),
    ]);
    assert_eq!(hooks[1]["matcher"], json!("^agent$"));
    assert_eq!(hooks[0]["sandbox"], json!(false));
}
