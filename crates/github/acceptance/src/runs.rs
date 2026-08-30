//! The corpus **run** battery's support: the run-time counterpart of the
//! lowering harness in the crate root.
//!
//! The sweep (`tests/runs.rs`, opt-in like the snapshot refresh) takes every
//! in-scope corpus workflow that lowers and runs it, measuring exactly the
//! runtime-tier surface — checkout, action staging and execution, the
//! artifact/cache backends, cross-job flow. Three transforms put a lowered
//! graph into sweep shape:
//!
//! - [`stub_run_scripts`]: every `run:` script becomes `true`. Real builds are
//!   out of the sweep's budget and beside its point — the metric is the
//!   runtime-tier surface, not the corpus projects' own builds — and `uses:`
//!   steps stay real. (The corpus fetch clones each repo's sources at the pin
//!   where the size cap allows, so version-file reads, local actions and
//!   git-dependent actions are real; only *build outputs* stay impossible.)
//! - [`containerize`]: every host scope is rewritten to a pinned runner image
//!   ([`battery_image`]), so corpus code never executes on the host.
//! - [`cap_expansions`]: every matrix is capped to its first leg. The metric is
//!   capability coverage, not leg multiplication.
//!
//! Classification separates *gap* from *server-coupled*: a first failure in a
//! step no local runner could ever satisfy (OIDC, GitHub App credentials,
//! third-party SaaS uploads, cross-run artifact downloads, repository secrets)
//! is an expected failure, kept out of the gap ranking — the same denominator
//! discipline that keeps REPORT.md honest.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::env::consts::ARCH;
use std::fmt::Write as _;

use frontend_gha::action::{
    ACTION_KIND, ActionLocation, DOCKER_ACTION_KIND, PinnedAction, RUN_KIND,
};
use frontend_gha::exprs::{has_env_sentinel, replace_env_sentinels};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{BinOp, ExpandTarget, Expansion, Expr, ExprId, Graph, NodeId, RuntimeTarget, Value};
use smol_str::SmolStr;

/// The pinned battery images: flavor tags (`:slim`) move after maintained
/// builds, so the battery pins flavor+commit — reproducible runs, refreshed
/// deliberately. From lithoscomputer/sandbox-images; multi-arch manifests
/// (linux/amd64 and linux/arm64), so the daemon runs its own architecture
/// natively and `runner.arch` reports it — an arm64 host is GitHub's
/// `ubuntu-*-arm` runner, an amd64 host its `ubuntu-*` one.
pub const RUNNER_IMAGE_2404: &str = "ghcr.io/lithoscomputer/ubuntu-24.04:slim-e38f48b4bcd5";
pub const RUNNER_IMAGE_2204: &str = "ghcr.io/lithoscomputer/ubuntu-22.04:slim-e38f48b4bcd5";
pub const RUNNER_IMAGE_2604: &str = "ghcr.io/lithoscomputer/ubuntu-26.04:slim-e38f48b4bcd5";

/// The dind flavor of the 24.04 runner: slim plus a Docker engine and its
/// `start-docker` helper. The daemon is not running when the container starts
/// — the session prologue brings it up lazily on the first step — and it needs
/// `--privileged` ([`privilege`]). Only the 24.04 flavor is built.
pub const RUNNER_IMAGE_2404_DIND: &str = "ghcr.io/lithoscomputer/ubuntu-24.04:dind-e38f48b4bcd5";

/// The full 24.04 runner capture — GitHub's own runner filesystem, ~20 GB to
/// pull once. Each architecture is a capture of GitHub's runner for that
/// architecture (`ubuntu-24.04` / `ubuntu-24.04-arm`) with its own
/// ImageVersion, so the pin is per architecture, chosen by the host's.
pub fn runner_image_2404_full() -> &'static str {
    match ARCH {
        "aarch64" => "ghcr.io/lithoscomputer/ubuntu-24.04-full:20260823.101.1-arm64",
        _ => "ghcr.io/lithoscomputer/ubuntu-24.04-full:20260823.283.1-amd64",
    }
}

/// Workflows the sweep runs on [`runner_image_2404_full`] instead of slim. On
/// GitHub they run on ubuntu-latest — the full image — and their setup-ruby
/// bundler step compiles native gems (libxml-ruby, mysql2) against packages
/// (`libxml2-dev`, `libmysqlclient-dev`) the slim image deliberately does not
/// carry: the additions were vetoed for their ICU weight (2026-08-29), and
/// slim stays at GitHub-ubuntu-slim parity. The full image wins over the dind
/// substitution for a listed workflow: a Docker-driving step then reports the
/// capture's own truth (binaries without a running daemon) rather than
/// failing on missing packages first.
pub const FULL_IMAGE_WORKFLOWS: &[(&str, &str)] = &[
    (
        "rails/rails",
        ".github/workflows/devcontainer-smoke-test.yml",
    ),
    ("rails/rails", ".github/workflows/rail_inspector.yml"),
    ("rails/rails", ".github/workflows/rails-new-docker.yml"),
];

/// Whether the sweep routes this workflow to the full image.
pub fn is_full_image_workflow(repo: &str, file: &str) -> bool {
    FULL_IMAGE_WORKFLOWS.contains(&(repo, file))
}

/// The battery image for a scope's placement labels: the label's OS version
/// picks the Ubuntu release, defaulting to 24.04 (`ubuntu-latest`).
pub fn battery_image(requirements: &[SmolStr]) -> &'static str {
    for label in requirements {
        if label.contains("22.04") {
            return RUNNER_IMAGE_2204;
        }
        if label.contains("26.04") {
            return RUNNER_IMAGE_2604;
        }
    }
    RUNNER_IMAGE_2404
}

/// Stub every `github/run` script to `true`, keeping the step's gate and
/// placement so control flow is exercised without executing corpus code.
///
/// The step's own `env` and `working-directory` go with the script: the env can
/// name repository secrets no local run has, and the directory may only exist
/// because a real script would have created it — both would fail a step whose
/// work is now `true`.
pub fn stub_run_scripts(graph: &mut Graph) {
    for node in &mut graph.nodes {
        if node.step.kind.as_ref() != RUN_KIND {
            continue;
        }
        let Some(config) = node.step.config.as_object_mut() else {
            continue;
        };
        config.insert("run".into(), Value::from("true"));
        config.insert("shell".into(), Value::from("sh"));
        config.remove("shell_command");
        config.remove("env");
        config.remove("working_dir");
    }
}

/// The nodes whose config consumes a stubbed `run:` step's output: a real
/// `uses:` step whose input reads `steps.<id>.outputs.*` of a script the sweep
/// stubbed to `true` — directly, or through an inlined composite whose declared
/// output wraps the inner script's record (both lower to the same
/// `nodes[...].output` read). The stub never writes the output, so the consumer
/// sees empty where GitHub sees a value; its failure is the stub's doing, not a
/// runtime gap, and the sweep classifies it as expected.
///
/// The read need not be direct. A stubbed step's output can travel through a
/// **job output** — the `{ result, outputs, index }` summary the job's last
/// edge carries into `<job>/done`, which `needs.<job>.outputs.<name>` projects
/// back out — and from there into another job's output, or into a matrix
/// expression, where a null leg poisons every clone that reads its `item`. All
/// of those consumers fail on the stub's account, so the taint is chased:
/// job outputs fed by stubbed steps to a fixpoint (an output built from
/// another job's tainted output is tainted too), then reads of tainted
/// outputs, then expansions whose items ride one.
pub fn stubbed_output_consumers(graph: &Graph) -> BTreeSet<String> {
    // The tainted job outputs, by `(<job>/done node, output name)`.
    type Tainted = BTreeSet<(String, String)>;

    // Every `$expr` placeholder in a config, by id.
    fn expr_ids(value: &Value, out: &mut Vec<ExprId>) {
        match value {
            Value::Object(map) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a `$expr` placeholder holds an `ExprId`, whose raw value is a u32"
                )]
                if let Some(raw) = map.get(EXPR_PLACEHOLDER_KEY).and_then(Value::as_u64) {
                    out.push(ExprId::new(raw as u32));
                }
                for child in map.values() {
                    expr_ids(child, out);
                }
            }
            Value::Array(items) => {
                for child in items {
                    expr_ids(child, out);
                }
            }
            _ => {}
        }
    }

    let stubbed: BTreeSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.step.kind.as_ref() == RUN_KIND)
        .map(|n| n.name.as_ref())
        .collect();

    // The node a `nodes.<name>` / `nodes[<name># + index]` record read names.
    let record_read = |base: ExprId| -> Option<String> {
        let is_nodes =
            |id: ExprId| matches!(graph.exprs.get(id), Some(Expr::Var(v)) if v == "nodes");
        match graph.exprs.get(base)? {
            Expr::Field(nodes, name) if is_nodes(*nodes) => Some(name.to_string()),
            Expr::Index(nodes, key) if is_nodes(*nodes) => match graph.exprs.get(*key)? {
                Expr::Binary(BinOp::Add, prefix, _) => match graph.exprs.get(*prefix)? {
                    Expr::Lit(Value::String(s)) => s.strip_suffix('#').map(str::to_string),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    };

    // The `(done node, output name)` a `<done>.output.outputs.<name>` read
    // projects — the lowered shape of `needs.<job>.outputs.<name>`.
    let outputs_projection = |id: ExprId| -> Option<(String, String)> {
        let Some(Expr::Field(outputs, name)) = graph.exprs.get(id) else {
            return None;
        };
        let Some(Expr::Field(output, outputs_key)) = graph.exprs.get(*outputs) else {
            return None;
        };
        if outputs_key != "outputs" {
            return None;
        }
        let Some(Expr::Field(record, output_key)) = graph.exprs.get(*output) else {
            return None;
        };
        if output_key != "output" {
            return None;
        }
        Some((record_read(*record)?, name.to_string()))
    };

    // Does the tree under `root` read a stubbed node's `output` — directly, or
    // as a tainted job output's projection?
    let reads_stubbed = |root: ExprId, tainted: &Tainted| -> bool {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(expr) = graph.exprs.get(id) else {
                continue;
            };
            if let Expr::Field(base, key) = expr
                && key == "output"
                && record_read(*base).is_some_and(|name| stubbed.contains(name.as_str()))
            {
                return true;
            }
            if let Some(pair) = outputs_projection(id)
                && tainted.contains(&pair)
            {
                return true;
            }
            push_children(expr, &mut stack);
        }
        false
    };

    // The tainted job outputs, to a fixpoint: every edge into a `<job>/done`
    // carries the job's summary — `{ result, outputs, index }` — and an
    // `outputs` entry reading a stubbed output (or an already-tainted one:
    // outputs pass through jobs) marks `(done, name)`.
    let mut tainted = Tainted::new();
    loop {
        let mut changed = false;
        for node in &graph.nodes {
            for edge in node.routing.edges() {
                let Some(map) = edge.map else {
                    continue;
                };
                let Some(target) = graph.node(edge.to) else {
                    continue;
                };
                if !target.name.ends_with("/done") {
                    continue;
                }
                let Some(Expr::Object(summary)) = graph.exprs.get(map) else {
                    continue;
                };
                let Some(outputs) = summary
                    .iter()
                    .find(|(k, _)| k == "outputs")
                    .map(|(_, v)| *v)
                else {
                    continue;
                };
                let Some(Expr::Object(entries)) = graph.exprs.get(outputs) else {
                    continue;
                };
                for (name, expr) in entries {
                    let pair = (target.name.to_string(), name.to_string());
                    if !tainted.contains(&pair) && reads_stubbed(*expr, &tainted) {
                        tainted.insert(pair);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    let config_reads = |node: &ir::Node, pred: &dyn Fn(ExprId) -> bool| -> bool {
        let mut ids = Vec::new();
        expr_ids(&node.step.config, &mut ids);
        ids.into_iter().any(pred)
    };

    let mut consumers = BTreeSet::new();
    for node in &graph.nodes {
        if node.step.kind.as_ref() == RUN_KIND {
            continue;
        }
        if config_reads(node, &|id| reads_stubbed(id, &tainted)) {
            consumers.insert(node.name.to_string());
        }
    }

    // The expansion hop: an expansion whose items ride a tainted output gets a
    // null where GitHub had a leg, so every node in its region reading the
    // leg's `item` — the lowered `matrix.*` — fails on the stub's account.
    // Region membership mirrors the engine's own splice rule (`region_nodes`
    // in the engine's apply): reachable from `entry` along routing edges
    // without passing through `exit`, plus `exit`.
    let region_of = |entry: NodeId, exit: NodeId| -> Vec<NodeId> {
        let mut seen = BTreeSet::from([entry]);
        let mut order = vec![entry];
        let mut queue = VecDeque::from([entry]);
        while let Some(id) = queue.pop_front() {
            if id == exit {
                continue;
            }
            let Some(node) = graph.node(id) else {
                continue;
            };
            for edge in node.routing.edges() {
                if graph.node(edge.to).is_some() && seen.insert(edge.to) {
                    order.push(edge.to);
                    queue.push_back(edge.to);
                }
            }
        }
        order
    };
    let reads_item = |root: ExprId| -> bool {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(expr) = graph.exprs.get(id) else {
                continue;
            };
            if matches!(expr, Expr::Var(v) if v == "item") {
                return true;
            }
            push_children(expr, &mut stack);
        }
        false
    };
    for node in &graph.nodes {
        let Some(Expansion::ForEach { items, target, .. }) = &node.expand else {
            continue;
        };
        if !reads_stubbed(*items, &tainted) {
            continue;
        }
        let region = match target {
            ExpandTarget::Node => vec![node.id],
            ExpandTarget::Subgraph { entry, exit } => region_of(*entry, *exit),
        };
        for id in region {
            let Some(member) = graph.node(id) else {
                continue;
            };
            if member.step.kind.as_ref() == RUN_KIND {
                continue;
            }
            if config_reads(member, &reads_item) {
                consumers.insert(member.name.to_string());
            }
        }
    }
    consumers
}

/// The nodes whose checkout `ref:` is built from a `workflow_dispatch` input
/// the sweep leaves empty. A directly run workflow's `inputs.<name>` lowers to
/// `default(github.event.inputs.<name>, <fallback>)`, and when the input
/// declares no default the fallback is the type's zero (`0`, `""`, `false`) —
/// GitHub's own rule for an unset input. The sweep's synthesized event carries
/// no inputs, so such a ref renders from zeros (`refs/pull/0/head`,
/// `v0.x-staging`) and names a ref that exists only for a real dispatch run —
/// server-coupled, not a gap. A ref read routed through scope `env:` is chased
/// to the env value's own expression. A ref reading only *defaulted* inputs
/// resolves to a real ref and is not collected: its failures stay gaps.
pub fn dispatch_ref_checkouts(graph: &Graph) -> BTreeSet<String> {
    // Does the ref expression read an empty-falling dispatch input, chasing
    // `env.<name>` into the scope's env expressions? `seen` breaks env cycles.
    fn reads_empty_input(
        graph: &Graph,
        scope: ir::ScopeId,
        root: ir::ExprId,
        seen: &mut BTreeSet<String>,
        event_inputs_default: &dyn Fn(&ir::Expr) -> bool,
        env_names: &dyn Fn(&ir::Expr) -> Vec<String>,
    ) -> bool {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(expr) = graph.exprs.get(id) else {
                continue;
            };
            if event_inputs_default(expr) {
                return true;
            }
            for name in env_names(expr) {
                if seen.insert(name.clone())
                    && scope_env_reads_empty_input(
                        graph,
                        scope,
                        &name,
                        seen,
                        event_inputs_default,
                        env_names,
                    )
                {
                    return true;
                }
            }
            push_children(expr, &mut stack);
        }
        false
    }

    // Chase one env-var name into the scope's own `env:` expression for it.
    fn scope_env_reads_empty_input(
        graph: &Graph,
        scope: ir::ScopeId,
        name: &str,
        seen: &mut BTreeSet<String>,
        event_inputs_default: &dyn Fn(&ir::Expr) -> bool,
        env_names: &dyn Fn(&ir::Expr) -> Vec<String>,
    ) -> bool {
        let Some(scope) = graph.scopes.iter().find(|s| s.id == scope) else {
            return false;
        };
        match scope.env.get(name) {
            Some(ir::ExprOrValue::Expr(env_expr)) => reads_empty_input(
                graph,
                scope.id,
                *env_expr,
                seen,
                event_inputs_default,
                env_names,
            ),
            _ => false,
        }
    }

    // Chase every env sentinel a string `ref:` carries. Each name is visited
    // once — the walk below adds more — and the first hit answers.
    fn sentinel_ref_reads_empty_input(
        graph: &Graph,
        scope: ir::ScopeId,
        text: &str,
        seen: &mut BTreeSet<String>,
        event_inputs_default: &dyn Fn(&ir::Expr) -> bool,
        env_names: &dyn Fn(&ir::Expr) -> Vec<String>,
    ) -> bool {
        for name in env_sentinel_names(text) {
            if !seen.insert(name.clone()) {
                continue;
            }
            if scope_env_reads_empty_input(
                graph,
                scope,
                &name,
                seen,
                event_inputs_default,
                env_names,
            ) {
                return true;
            }
        }
        false
    }

    let zero = |v: &Value| match v {
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Bool(b) => !b,
        _ => false,
    };
    let lit_str = |id: ExprId, want: &str| matches!(graph.exprs.get(id), Some(Expr::Lit(Value::String(s))) if s == want);
    // `get_ci(get_ci(github, "event"), "inputs")` — the one read
    // `bind_param_inputs` builds for a run-parameter lookup.
    let event_inputs = |id: ExprId| -> bool {
        let Some(Expr::Call(name, args)) = graph.exprs.get(id) else {
            return false;
        };
        name == "get_ci"
            && args.len() == 2
            && lit_str(args[1], "inputs")
            && match graph.exprs.get(args[0]) {
                Some(Expr::Call(inner_name, inner)) => {
                    inner_name == "get_ci"
                        && inner.len() == 2
                        && matches!(graph.exprs.get(inner[0]), Some(Expr::Var(v)) if v == "github")
                        && lit_str(inner[1], "event")
                }
                _ => false,
            }
    };
    // Does this subtree contain the run-parameter read at all?
    let contains_event_inputs = |root: ExprId| -> bool {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if event_inputs(id) {
                return true;
            }
            if let Some(expr) = graph.exprs.get(id) {
                push_children(expr, &mut stack);
            }
        }
        false
    };
    // The env-var names an expression reads, in every lowered spelling: an
    // engine-side `env.<name>` read (under a function or operator), or the env
    // sentinels a bare `${{ env.NAME }}` leaves in a string literal for the
    // step to substitute at spawn.
    let env_names = |expr: &Expr| -> Vec<String> {
        let is_env = |id: ExprId| matches!(graph.exprs.get(id), Some(Expr::Var(v)) if v == "env");
        match expr {
            Expr::Field(base, name) if is_env(*base) => vec![name.to_string()],
            Expr::Index(base, key) if is_env(*base) => match graph.exprs.get(*key) {
                Some(Expr::Lit(Value::String(s))) => vec![s.clone()],
                _ => Vec::new(),
            },
            Expr::Call(name, args) if name == "get_ci" && args.len() == 2 && is_env(args[0]) => {
                match graph.exprs.get(args[1]) {
                    Some(Expr::Lit(Value::String(s))) => vec![s.clone()],
                    _ => Vec::new(),
                }
            }
            Expr::Lit(Value::String(s)) => env_sentinel_names(s),
            _ => Vec::new(),
        }
    };

    // `default(<reads github.event.inputs>, <zero literal>)`: an undeclared
    // default zero-fills exactly here; a declared default puts its real value
    // in the fallback slot and does not match.
    let event_inputs_default = |expr: &Expr| -> bool {
        let Expr::Call(name, args) = expr else {
            return false;
        };
        name == "default"
            && args.len() == 2
            && matches!(graph.exprs.get(args[1]), Some(Expr::Lit(v)) if zero(v))
            && contains_event_inputs(args[0])
    };

    let mut out = BTreeSet::new();
    for node in &graph.nodes {
        if node.step.kind.as_ref() != ACTION_KIND {
            continue;
        }
        let StepIdentity::Action { bare, .. } = action_identity(&node.step.config) else {
            continue;
        };
        if bare != "actions/checkout" {
            continue;
        }
        let Some(reference) = node.step.config.get("inputs").and_then(|i| i.get("ref")) else {
            continue;
        };
        let mut seen = BTreeSet::new();
        // The ref is a placeholder where the engine evaluates it, or a string
        // whose only expressions were bare `env.NAME` reads, left as sentinels.
        let coupled = match reference.get(EXPR_PLACEHOLDER_KEY).and_then(Value::as_u64) {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a `$expr` placeholder holds an `ExprId`, whose raw value is a u32"
            )]
            Some(raw) => reads_empty_input(
                graph,
                node.scope,
                ir::ExprId::new(raw as u32),
                &mut seen,
                &event_inputs_default,
                &env_names,
            ),
            None => reference.as_str().is_some_and(|text| {
                sentinel_ref_reads_empty_input(
                    graph,
                    node.scope,
                    text,
                    &mut seen,
                    &event_inputs_default,
                    &env_names,
                )
            }),
        };
        if coupled {
            out.insert(node.name.to_string());
        }
    }
    out
}

/// The names of every env sentinel in `text` — a bare `${{ env.NAME }}` in
/// step config, left for the step to substitute at spawn.
fn env_sentinel_names(text: &str) -> Vec<String> {
    if !has_env_sentinel(text) {
        return Vec::new();
    }
    let mut names = Vec::new();
    let _ = replace_env_sentinels(text, |name| -> Result<String, Infallible> {
        names.push(name.to_string());
        Ok(String::new())
    });
    names
}

/// Every child expression of `expr`, onto `stack`.
fn push_children(expr: &ir::Expr, stack: &mut Vec<ir::ExprId>) {
    match expr {
        Expr::Lit(_) | Expr::Var(_) => {}
        Expr::Field(a, _) | Expr::Unary(_, a) => stack.push(*a),
        Expr::Index(a, b) | Expr::Binary(_, a, b) => stack.extend([*a, *b]),
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => stack.extend([*cond, *then, *otherwise]),
        Expr::Array(items) => stack.extend(items.iter().copied()),
        Expr::Object(pairs) => stack.extend(pairs.iter().map(|(_, v)| *v)),
        Expr::Call(_, args) => stack.extend(args.iter().copied()),
    }
}

/// Rewrite every host scope to a runner container, chosen per scope from its
/// placement labels. A workflow's own `container:` stays its own image — the
/// stand-in is only for scopes that would have run on the host. With no
/// `platform` the daemon runs its native architecture; `Some("linux/amd64")`
/// forces GitHub's x64 runner shape under emulation, for a fidelity
/// comparison against the hosted default.
pub fn containerize(
    graph: &mut Graph,
    platform: Option<&str>,
    image_for: impl Fn(&[SmolStr]) -> String,
) {
    for scope in &mut graph.scopes {
        if !matches!(scope.runtime.target, RuntimeTarget::HostProcess) {
            continue;
        }
        let image = image_for(&scope.runtime.requirements);
        scope.runtime.target = RuntimeTarget::Container {
            image:       SmolStr::new(&image),
            options:     platform
                .map(|p| vec![SmolStr::new("--platform"), SmolStr::new(p)])
                .unwrap_or_default(),
            credentials: None,
        };
    }
}

/// Does this graph drive a Docker engine? Docker container actions run
/// `docker` over the scope's runner, and the `docker/*` toolchain actions
/// (login, setup-buildx, build-push, …) call the CLI directly. `run:` scripts
/// don't count — the sweep stubs them to `true`. Service containers don't
/// either: the executor starts those on the outer daemon at acquisition.
pub fn needs_docker(graph: &Graph) -> bool {
    step_identities(graph)
        .values()
        .any(|identity| match identity {
            StepIdentity::DockerAction(_) => true,
            StepIdentity::Action { bare, .. } => bare.starts_with("docker/"),
            _ => false,
        })
}

/// Add `--privileged` to every containerized scope running `image`: the dind
/// runner's Docker engine cannot start without it. A separate pass after
/// [`containerize`] so the flag rides exactly the scopes that got that image.
pub fn privilege(graph: &mut Graph, image: &str) {
    for scope in &mut graph.scopes {
        if let RuntimeTarget::Container {
            image: scope_image,
            options,
            ..
        } = &mut scope.runtime.target
            && scope_image == image
        {
            options.push(SmolStr::new("--privileged"));
        }
    }
}

/// Cap every expansion — static and expression matrices alike — to its first
/// leg, by rewriting the items expression to `[items[0]]`.
pub fn cap_expansions(graph: &mut Graph) {
    for index in 0..graph.nodes.len() {
        let Some(Expansion::ForEach { items, .. }) = &graph.nodes[index].expand else {
            continue;
        };
        let items = *items;
        let zero = graph.exprs.lit(0);
        let first = graph.exprs.index(items, zero);
        let capped = graph.exprs.array(vec![first]);
        if let Some(Expansion::ForEach { items, .. }) = &mut graph.nodes[index].expand {
            *items = capped;
        }
    }
}

/// What a graph node is, for reading a first failure: the `uses:` reference
/// behind it, or the step family when there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepIdentity {
    /// A remote action: the bare `owner/repo[/path]` and whether this call
    /// reads another run's artifacts (`download-artifact` with `run-id:`).
    Action { bare: String, cross_run: bool },
    /// A local (`./`) action step.
    LocalAction(String),
    /// A Docker container action (`docker://` or a Dockerfile action).
    DockerAction(String),
    /// A `run:` script — stubbed in the sweep, so a failure here is the
    /// harness's own.
    Run,
    /// Anything else (job start/done markers and other structure).
    Other(String),
}

impl StepIdentity {
    /// The label the report ranks by.
    pub fn label(&self) -> String {
        match self {
            Self::Action { bare, .. } => bare.clone(),
            Self::LocalAction(path) => format!("./{}", path.trim_start_matches("./")),
            Self::DockerAction(image) => image.clone(),
            Self::Run => "run:".to_string(),
            // A job's start/done marker: the failure is the job's environment,
            // not any step's.
            Self::Other(kind) if kind == "noop" => "job marker".to_string(),
            Self::Other(kind) => kind.clone(),
        }
    }
}

/// Node name → identity for every node in the graph. Runtime clones are named
/// `{name}#{index}`; [`identity_of`] strips that before looking here.
pub fn step_identities(graph: &Graph) -> BTreeMap<String, StepIdentity> {
    let mut out = BTreeMap::new();
    for node in &graph.nodes {
        let identity = match node.step.kind.as_ref() {
            ACTION_KIND => action_identity(&node.step.config),
            DOCKER_ACTION_KIND => docker_identity(&node.step.config),
            RUN_KIND => StepIdentity::Run,
            other => StepIdentity::Other(other.to_string()),
        };
        out.insert(node.name.to_string(), identity);
    }
    out
}

/// The identity for a firing-record name: exact match, else with the splice's
/// `#index` clone suffix stripped from each path segment.
pub fn identity_of<'a>(
    identities: &'a BTreeMap<String, StepIdentity>,
    record_name: &str,
) -> Option<&'a StepIdentity> {
    if let Some(found) = identities.get(record_name) {
        return Some(found);
    }
    identities.get(&clone_base(record_name))
}

/// The template name behind a firing-record name: the splice's `#index` clone
/// suffix stripped from **each** path segment — a nested clone
/// (`job#0/step#1`) suffixes every level, so a first-`#` cut is wrong.
pub fn clone_base(record_name: &str) -> String {
    record_name
        .split('/')
        .map(strip_clone_suffix)
        .collect::<Vec<_>>()
        .join("/")
}

fn strip_clone_suffix(segment: &str) -> &str {
    match segment.rsplit_once('#') {
        Some((name, index)) if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => segment,
    }
}

fn action_identity(config: &Value) -> StepIdentity {
    let Some(location) = config
        .get("action")
        .and_then(|a| serde_json::from_value::<ActionLocation>(a.clone()).ok())
    else {
        return StepIdentity::Other(ACTION_KIND.to_string());
    };
    match location {
        ActionLocation::Pinned(pinned) => StepIdentity::Action {
            cross_run: reads_another_run(&pinned, config),
            bare:      bare_reference(&pinned),
        },
        ActionLocation::Local { local } => StepIdentity::LocalAction(local),
    }
}

fn docker_identity(config: &Value) -> StepIdentity {
    match config.get("image") {
        Some(image) => {
            if let Some(registry) = image.get("registry").and_then(Value::as_str) {
                StepIdentity::DockerAction(registry.to_string())
            } else {
                StepIdentity::DockerAction("Dockerfile".to_string())
            }
        }
        None => StepIdentity::Other(DOCKER_ACTION_KIND.to_string()),
    }
}

/// `owner/repo[/path]` without the `@ref`.
fn bare_reference(pinned: &PinnedAction) -> String {
    let full = pinned.reference.to_string();
    full.split('@').next().unwrap_or(&full).to_string()
}

/// `download-artifact` with a `run-id:` input reads another workflow run's
/// artifacts over the REST API — a hosted-backend coupling no local run store
/// can answer.
fn reads_another_run(pinned: &PinnedAction, config: &Value) -> bool {
    if !bare_reference(pinned).ends_with("download-artifact") {
        return false;
    }
    config
        .get("inputs")
        .and_then(Value::as_object)
        .is_some_and(|inputs| inputs.contains_key("run-id"))
}

/// Actions whose work is inseparable from a hosted service — OIDC issuers,
/// GitHub App credentials, SaaS backends, and **mutating actions**, whose whole
/// job is a write to github.com: a local run with write permission would
/// mutate the real repository (the API stance), so the sweep, which must never
/// mutate, can never see them pass. No local runner can fix a failure here,
/// so the sweep classifies it as expected rather than a gap.
pub const SERVER_COUPLED: &[(&str, &str)] = &[
    ("actions/attest-build-provenance", "OIDC attestation"),
    ("open-security-tools/ost-simple-sts", "OIDC token exchange"),
    ("google-github-actions/auth", "OIDC cloud auth"),
    ("aws-actions/configure-aws-credentials", "OIDC cloud auth"),
    ("azure/login", "OIDC cloud auth"),
    ("actions/create-github-app-token", "GitHub App credentials"),
    (
        "github/codeql-action/upload-sarif",
        "uploads to GitHub code scanning",
    ),
    ("codecov/codecov-action", "third-party SaaS upload"),
    ("coverallsapp/github-action", "third-party SaaS upload"),
    ("CodSpeedHQ/action", "third-party SaaS upload"),
    (
        "depot/build-push-action",
        "builds on the depot.dev service (project token)",
    ),
    ("rust-lang/crates-io-auth-action", "OIDC token exchange"),
    (
        "rubygems/configure-rubygems-credentials",
        "OIDC token exchange",
    ),
    ("dessant/lock-threads", "mutates issues over the API"),
    ("github/issue-labeler", "mutates issues over the API"),
    (
        "release-drafter/release-drafter",
        "mutates releases over the API",
    ),
    (
        "gr2m/create-or-update-pull-request-action",
        "mutates pull requests over the API",
    ),
    (
        "JamesIves/github-pages-deploy-action",
        "pushes a deployment branch",
    ),
    (
        "actions/publish-immutable-action",
        "publishes to GitHub's registry",
    ),
    (
        "mheap/github-action-required-labels",
        "reads the triggering pull request (no local event)",
    ),
];

/// The step-kind class a step reports when a `$secret` reference has no value.
/// Spelled here rather than imported: the lib half of this crate stays off the
/// runtime, and the string is a stable step-kind contract.
const SECRET_UNAVAILABLE: &str = "secret_unavailable";

/// Why this first failure is expected — server-coupled, not a gap — or `None`
/// when it measures a real runtime-tier gap. `authenticated` says whether the
/// sweep ran with a real token: with one, only the credential-free classes
/// (OIDC, other repositories' secrets, SaaS backends) stay expected.
pub fn expected_reason(
    identity: &StepIdentity,
    failure_class: &str,
    authenticated: bool,
) -> Option<String> {
    if failure_class == SECRET_UNAVAILABLE {
        return Some("needs a repository secret".to_string());
    }
    let StepIdentity::Action { bare, cross_run } = identity else {
        return None;
    };
    if *cross_run {
        return Some("cross-run artifact download (REST API)".to_string());
    }
    // The real checkout action requires a token input and authenticates its
    // fetch with it, so a token-less sweep cannot run it at all. Local
    // checkout substitution is the local answer; what falls through to the
    // real action needs a credential — and with one supplied, a failure there
    // is a real gap again.
    if !authenticated && bare == "actions/checkout" && failure_class.starts_with("exit_status") {
        return Some("needs a git credential (the sweep ran token-less)".to_string());
    }
    SERVER_COUPLED
        .iter()
        .find(|(action, _)| action == bare)
        .map(|(_, why)| why.to_string())
}

/// Why a first failure is expected based on what the step actually said —
/// the second look, for classes only the log can name. `log` is the failing
/// step's whole log, not the report's display tail: the line that names the
/// cause can sit hundreds of lines before the end (scorecard prints its error,
/// then dumps the full results JSON). One family: an artifact upload of a
/// *build output* (`dist/`, a bundle) that the sweep's stubbed `run:` scripts
/// structurally never produce. Sources are real where the corpus fetched them;
/// build outputs never are.
pub fn expected_from_log(identity: &StepIdentity, log: &[String]) -> Option<String> {
    // ENOSYS from basic syscalls (`mkdir: Function not implemented`) means the
    // image's binaries don't run under this host's emulation: a workflow's own
    // `container:` image built for one architecture only, pulled through the
    // executor's platform fallback on a host of the other. A host-architecture
    // limit, not a petri gap — a host of the image's architecture runs these
    // rows. Any step can hit it, so this look precedes the action-only ones.
    if log
        .iter()
        .any(|line| line.contains("Function not implemented"))
    {
        return Some("amd64-only image; this host's emulation cannot run it".to_string());
    }
    // The scorecard action's publish path signs its results, and the signing
    // service takes only the Actions-issued ephemeral `GITHUB_TOKEN` — any
    // PAT, which is all the sweep can supply, is rejected on shape alone. The
    // scan itself completed; the write to the OpenSSF service is what failed.
    if let StepIdentity::DockerAction(image) = identity
        && image.starts_with("ghcr.io/ossf/scorecard-action")
        && log
            .iter()
            .any(|line| line.contains("SigningNew: invalid token"))
    {
        return Some("signs results with the Actions-issued GITHUB_TOKEN".to_string());
    }
    let StepIdentity::Action { bare, .. } = identity else {
        return None;
    };
    if bare.ends_with("upload-artifact")
        && log
            .iter()
            .any(|line| line.contains("No files were found with the provided path"))
    {
        return Some("uploads outputs a stubbed build never produced".to_string());
    }
    // paths-filter run against no repository: on GitHub a pull-request event
    // routes it to the PR list-files API — the job checks nothing out — while
    // the sweep's synthesized event forces its git-diff mode, which fatals on
    // the empty workspace. Two keys, both required: git's no-repository fatal,
    // and the log's last error being the git call that hit it — a stray
    // mention with a different terminal failure classifies nothing, and a
    // git-mode diff failing over a real checkout stays a gap.
    if bare == "dorny/paths-filter"
        && log.iter().any(|line| line.contains("not a git repository"))
        && error_line(log).is_some_and(|line| line.contains("The process 'git"))
    {
        return Some("reads the triggering pull request (no local event)".to_string());
    }
    // codeql analyze reads its *own* workflow run over the Actions API to
    // decide status-report fields; petri's run id names no server-side run, so
    // the lookup 404s. The same 404 also shows up incidentally in logs that
    // die on something else (cli/cli's database-finalize fatal prints one on
    // the way down), so a mere mention is not the cause: the 404 must be the
    // log's last error-naming line — the failure the step actually reported.
    if bare == "github/codeql-action/analyze"
        && error_line(log).is_some_and(|line| line.contains("workflow-runs#get-a-workflow-run"))
    {
        return Some("reads its own workflow run (no server-side run exists)".to_string());
    }
    // An unhandled exception inside a `github-script` inline script is the
    // script's own business, never the runner's: the runner staged it, ran
    // node, and handed it the toolkit. Locally these scripts break on the
    // empty `github.event` — they act on the triggering pull request or issue
    // (`context.payload.number`, `.labels`), which a local run does not have.
    // GitHub with the same empty event would produce the identical throw:
    // interpolation is not the difference, the event is.
    if bare == "actions/github-script" && log.iter().any(|line| line.contains("Unhandled error:")) {
        return Some(
            "the inline script threw (it acts on the triggering event, which a local run lacks)"
                .to_string(),
        );
    }
    None
}

// ── The report ────────────────────────────────────────────────────────────

/// One workflow's sweep result.
#[derive(Clone, Debug)]
pub struct RunRecord {
    pub repo:   String,
    pub file:   String,
    pub result: RunResult,
}

#[derive(Clone, Debug)]
pub enum RunResult {
    /// The run reached `Success`.
    Pass,
    /// The run failed; this is its first failing step.
    Fail(FirstFailure),
    /// The wall-clock cap fired. `wedged` means even the cancel did not bring
    /// the run down and it was abandoned.
    TimedOut { wedged: bool },
    /// In scope but rejected at lowering — carried for the denominator, never
    /// run.
    NotLowered { features: Vec<String> },
}

/// The first failing step of a failed run.
#[derive(Clone, Debug)]
pub struct FirstFailure {
    /// The firing-record name, as recorded.
    pub node:     String,
    /// The step behind it — [`StepIdentity::label`].
    pub step:     String,
    /// The failure class the step reported (empty when unclassified).
    pub class:    String,
    pub message:  String,
    /// The step's last log lines — what the failure actually said — plus the
    /// last [`error_line`] when later noise pushed it out of the window.
    pub tail:     Vec<String>,
    /// Present when the failure is server-coupled ([`expected_reason`]).
    pub expected: Option<String>,
}

impl RunResult {
    fn label(&self) -> String {
        match self {
            Self::Pass => "pass".to_string(),
            Self::Fail(f) if f.expected.is_some() => "expected failure".to_string(),
            Self::Fail(_) => "**fail**".to_string(),
            Self::TimedOut { wedged: false } => "**timeout**".to_string(),
            Self::TimedOut { wedged: true } => "**timeout (wedged)**".to_string(),
            Self::NotLowered { .. } => "not lowered".to_string(),
        }
    }
}

/// `RUNS.md`: the sweep's outcome per workflow plus first-failure classes
/// ranked — the run-time REPORT.md. `note` states this sweep's configuration
/// (image pin, caps, identity), written by the sweep that measured.
pub fn runs_report(records: &[RunRecord], note: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Corpus run sweep\n");
    let _ = writeln!(
        out,
        "The run-time counterpart of REPORT.md: every in-scope workflow that lowers, \
         run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. \
         The outcome measures the runtime tier — checkout, action staging and \
         execution, artifact and cache backends, cross-job flow — not the corpus \
         projects' own builds.\n"
    );
    let _ = writeln!(out, "{note}\n");

    let total = records.len();
    let not_lowered = records
        .iter()
        .filter(|r| matches!(r.result, RunResult::NotLowered { .. }))
        .count();
    let ran = total - not_lowered;
    let pass = records
        .iter()
        .filter(|r| matches!(r.result, RunResult::Pass))
        .count();
    let expected = records
        .iter()
        .filter(|r| matches!(&r.result, RunResult::Fail(f) if f.expected.is_some()))
        .count();
    let gaps = records
        .iter()
        .filter(|r| matches!(&r.result, RunResult::Fail(f) if f.expected.is_none()))
        .count();
    let timeouts = records
        .iter()
        .filter(|r| matches!(r.result, RunResult::TimedOut { .. }))
        .count();
    let _ = writeln!(
        out,
        "{total} workflows in scope; {ran} lower and were run.\n"
    );
    let _ = writeln!(out, "| Result (of the {ran} run) | Count | Share |");
    let _ = writeln!(out, "|---|---|---|");
    let share = |n: usize| {
        if ran == 0 {
            0.0
        } else {
            100.0 * n as f64 / ran as f64
        }
    };
    for (label, n) in [
        ("passed", pass),
        ("**failed on a runtime-tier gap**", gaps),
        ("expected failure (server-coupled)", expected),
        ("**timed out**", timeouts),
    ] {
        let _ = writeln!(out, "| {label} | {n} | {:.0}% |", share(n));
    }

    // Gap classes ranked: what to build next, by how much it blocks.
    let mut gap_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut expected_classes: BTreeMap<String, usize> = BTreeMap::new();
    for record in records {
        if let RunResult::Fail(f) = &record.result {
            let key = if f.class.is_empty() {
                f.step.clone()
            } else {
                format!("{} · {}", f.step, f.class)
            };
            match &f.expected {
                Some(why) => {
                    *expected_classes
                        .entry(format!("{} · {why}", f.step))
                        .or_default() += 1;
                }
                None => *gap_classes.entry(key).or_default() += 1,
            }
        }
    }
    let ranked = |classes: &BTreeMap<String, usize>| {
        let mut ranked: Vec<(String, usize)> =
            classes.iter().map(|(k, v)| (k.clone(), *v)).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
    };
    let _ = writeln!(out, "\n## First failures — gaps, ranked\n");
    if gap_classes.is_empty() {
        let _ = writeln!(out, "None.");
    } else {
        let _ = writeln!(out, "| First failing step · class | Workflows |");
        let _ = writeln!(out, "|---|---|");
        for (key, n) in ranked(&gap_classes) {
            let _ = writeln!(out, "| `{key}` | {n} |");
        }
    }
    let _ = writeln!(out, "\n## Expected failures — server-coupled, ranked\n");
    let _ = writeln!(
        out,
        "First failures the sweep's own stance produces or no token-less local \
         run can fix: OIDC, GitHub App and repository secrets, git credentials \
         for the real checkout action, third-party SaaS backends, cross-run \
         artifact reads, steps that read an output a stubbed `run:` script \
         would have written, refs and event payloads only a real triggering \
         run has, and actions that read or write only the hosted service can \
         answer. Kept out of the gap ranking so it never drowns in \
         server-bound noise.\n"
    );
    if expected_classes.is_empty() {
        let _ = writeln!(out, "None.");
    } else {
        let _ = writeln!(out, "| First failing step · why | Workflows |");
        let _ = writeln!(out, "|---|---|");
        for (key, n) in ranked(&expected_classes) {
            let _ = writeln!(out, "| `{key}` | {n} |");
        }
    }

    // Per-workflow inventory.
    let _ = writeln!(out, "\n## Every workflow\n");
    let _ = writeln!(out, "| Repository | Workflow | Result | First failure |");
    let _ = writeln!(out, "|---|---|---|---|");
    for record in records {
        let detail = match &record.result {
            RunResult::Fail(f) => {
                let mut detail = format!("`{}` — {}", f.step, sanitize_cell(&f.message));
                let named = error_line(&f.tail).or_else(|| {
                    f.tail
                        .iter()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .map(String::as_str)
                });
                if let Some(line) = named {
                    let _ = write!(detail, " · `{}`", sanitize_cell(line));
                }
                if let Some(why) = &f.expected {
                    let _ = write!(detail, " _({why})_");
                }
                detail
            }
            RunResult::NotLowered { features } if !features.is_empty() => features
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "—".to_string(),
        };
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} |",
            record.repo,
            record.file.trim_start_matches(".github/workflows/"),
            record.result.label(),
            detail
        );
    }
    out
}

/// The last line that names an error, if any: raw `Error:`/`error:` tool
/// output, a `::error::` command the runner rendered as `Error:`, or a nested
/// runner's `##[error]`. Trailing noise — a deprecation warning's continuation,
/// a stack frame — often outlives the error itself, so "last line" is not it.
pub fn error_line(lines: &[String]) -> Option<&str> {
    lines
        .iter()
        .rev()
        .find(|line| {
            let l = line.trim_start();
            l.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("error:"))
                || l.starts_with("##[error]")
                || l.starts_with("::error")
        })
        .map(String::as_str)
}

/// One Markdown table cell: no pipes, no newlines, bounded length.
fn sanitize_cell(text: &str) -> String {
    let mut cell: String = text
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
        .chars()
        .take(200)
        .collect();
    if cell.len() < text.len() {
        cell.push('…');
    }
    cell
}

#[cfg(test)]
mod tests {
    use frontend_gha::action::{ActionRef, ActionSource, ActionSourceError};
    use frontend_gha::load;
    use ir::Expr;

    use super::*;

    fn lower(text: &str) -> Graph {
        let lowered = load(".github/workflows/test.yml", text, &frontend::NoFiles);
        lowered.graph.expect("the test workflow lowers")
    }

    /// A resolver for tests whose workflows call remote actions: every
    /// reference pins as-is, every manifest is a minimal node action.
    struct StubActions;

    impl ActionSource for StubActions {
        fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
            Ok(PinnedAction {
                reference: reference.clone(),
                sha:       "0123456789012345678901234567890123456789".into(),
            })
        }

        fn manifest(&self, _pinned: &PinnedAction) -> Result<String, ActionSourceError> {
            Ok("name: checkout\n\
                inputs:\n\
                \x20 ref: { description: r }\n\
                \x20 path: { description: p }\n\
                runs:\n\
                \x20 using: node20\n\
                \x20 main: index.js\n"
                .to_string())
        }
    }

    fn lower_uses(text: &str) -> Graph {
        let lowered = frontend_gha::load_with(
            ".github/workflows/test.yml",
            text,
            &frontend::NoFiles,
            Some(&StubActions),
        );
        lowered.graph.expect("the test workflow lowers")
    }

    /// A real `uses:` step whose input reads a `run:` step's output is a
    /// stubbed-output consumer — the stub will never write the output. Steps
    /// with no such read are not.
    #[test]
    fn output_consumers_of_stubbed_scripts_are_found() {
        let graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 j:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   steps:\n\
             \x20     - id: v\n\
             \x20       run: echo \"version=1\" >> \"$GITHUB_OUTPUT\"\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: ${{ steps.v.outputs.version }}\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: fixed\n",
        );
        let consumers = stubbed_output_consumers(&graph);
        assert!(
            consumers.iter().any(|n| n.contains("step-2")),
            "the reader of `steps.v.outputs.version` is a consumer: {consumers:?}"
        );
        assert!(
            !consumers.iter().any(|n| n.contains("step-3")),
            "a literal `args:` consumes nothing: {consumers:?}"
        );
    }

    /// A stubbed step's output rides the job summary into `<job>/done` and
    /// comes back out as `needs.<job>.outputs.<name>`: its reader is a
    /// consumer even a job removed (the taint passes through `relay`'s own
    /// output), while an output no stubbed step feeds taints nothing.
    #[test]
    fn job_outputs_of_stubbed_scripts_taint_their_readers() {
        let graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 make:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   outputs:\n\
             \x20     version: ${{ steps.v.outputs.version }}\n\
             \x20     fixed: plain\n\
             \x20   steps:\n\
             \x20     - id: v\n\
             \x20       run: echo \"version=1\" >> \"$GITHUB_OUTPUT\"\n\
             \x20 relay:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   needs: make\n\
             \x20   outputs:\n\
             \x20     forwarded: ${{ needs.make.outputs.version }}\n\
             \x20   steps:\n\
             \x20     - run: true\n\
             \x20 reads-stubbed:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   needs: relay\n\
             \x20   steps:\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: ${{ needs.relay.outputs.forwarded }}\n\
             \x20 reads-fixed:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   needs: make\n\
             \x20   steps:\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: ${{ needs.make.outputs.fixed }}\n",
        );
        let consumers = stubbed_output_consumers(&graph);
        assert!(
            consumers.iter().any(|n| n.starts_with("reads-stubbed/")),
            "the reader of a stubbed job output, one relay removed: {consumers:?}"
        );
        assert!(
            !consumers.iter().any(|n| n.starts_with("reads-fixed/")),
            "a job output no stubbed step feeds taints nothing: {consumers:?}"
        );
    }

    /// A matrix whose items expression rides a stubbed job output expands over
    /// a null leg: every node in the expansion region reading the leg's `item`
    /// (the lowered `matrix.*`) is a consumer. A literal input in the same
    /// region reads no item, and a static matrix's readers get real legs.
    #[test]
    fn tainted_matrix_expansions_mark_their_item_readers() {
        let graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 gen:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   outputs:\n\
             \x20     matrix: ${{ steps.m.outputs.matrix }}\n\
             \x20   steps:\n\
             \x20     - id: m\n\
             \x20       run: echo \"matrix=[1]\" >> \"$GITHUB_OUTPUT\"\n\
             \x20 fan:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   needs: gen\n\
             \x20   strategy:\n\
             \x20     matrix: ${{ fromJSON(needs.gen.outputs.matrix) }}\n\
             \x20   steps:\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: ${{ matrix.value }}\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: literal\n\
             \x20 fixed:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   strategy:\n\
             \x20     matrix: { v: [1, 2] }\n\
             \x20   steps:\n\
             \x20     - uses: docker://alpine:3.20\n\
             \x20       with:\n\
             \x20         args: ${{ matrix.v }}\n",
        );
        let consumers = stubbed_output_consumers(&graph);
        assert!(
            consumers
                .iter()
                .any(|n| n.starts_with("fan/") && n.contains("step-1")),
            "the item reader under a tainted matrix is a consumer: {consumers:?}"
        );
        assert!(
            !consumers
                .iter()
                .any(|n| n.starts_with("fan/") && n.contains("step-2")),
            "a literal input in the region reads no item: {consumers:?}"
        );
        assert!(
            !consumers.iter().any(|n| n.starts_with("fixed/")),
            "a static matrix's legs are real: {consumers:?}"
        );
    }

    /// A checkout ref built from a defaultless dispatch input is collected —
    /// the sweep's empty event zero-fills it into a ref no repository has. A
    /// ref reading a *defaulted* input resolves to a real ref, and a literal
    /// ref reads nothing: both stay out, so their failures stay gaps.
    #[test]
    fn zero_filled_dispatch_refs_are_found_and_defaulted_ones_are_not() {
        let pin = "0123456789012345678901234567890123456789";
        let graph = lower_uses(&format!(
            "on:\n\
             \x20 workflow_dispatch:\n\
             \x20   inputs:\n\
             \x20     pull_request: {{ required: true, type: number }}\n\
             \x20     branch: {{ type: string, default: main }}\n\
             jobs:\n\
             \x20 j:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   steps:\n\
             \x20     - uses: actions/checkout@{pin}\n\
             \x20       with:\n\
             \x20         ref: refs/pull/${{{{ inputs.pull_request }}}}/head\n\
             \x20     - uses: actions/checkout@{pin}\n\
             \x20       with:\n\
             \x20         ref: ${{{{ inputs.branch }}}}\n\
             \x20     - uses: actions/checkout@{pin}\n\
             \x20       with:\n\
             \x20         ref: main\n",
        ));
        let found = dispatch_ref_checkouts(&graph);
        assert!(
            found.iter().any(|n| n.contains("step-1")),
            "the zero-filled ref is collected: {found:?}"
        );
        assert!(
            !found.iter().any(|n| n.contains("step-2")),
            "a defaulted input resolves to a real ref: {found:?}"
        );
        assert!(
            !found.iter().any(|n| n.contains("step-3")),
            "a literal ref reads nothing: {found:?}"
        );
    }

    /// The nodejs shape: the input read hides behind a job-level `env:`
    /// (`ref: ${{ env.STAGING }}`, `STAGING: v${{ inputs.release-line }}.x`).
    /// The walker chases the env value's own expression.
    #[test]
    fn dispatch_ref_reads_chase_scope_env() {
        let pin = "0123456789012345678901234567890123456789";
        let graph = lower_uses(&format!(
            "on:\n\
             \x20 workflow_dispatch:\n\
             \x20   inputs:\n\
             \x20     release-line: {{ required: true, type: number }}\n\
             jobs:\n\
             \x20 j:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   env:\n\
             \x20     STAGING: v${{{{ inputs.release-line }}}}.x-staging\n\
             \x20   steps:\n\
             \x20     - uses: actions/checkout@{pin}\n\
             \x20       with:\n\
             \x20         ref: ${{{{ env.STAGING }}}}\n",
        ));
        let found = dispatch_ref_checkouts(&graph);
        assert!(
            found.iter().any(|n| n.contains("step-1")),
            "the env-routed input read is chased: {found:?}"
        );
    }

    #[test]
    fn stubbed_scripts_lose_env_shell_and_cwd() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 build:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   steps:\n\
             \x20     - run: import sys\n\
             \x20       shell: python\n\
             \x20       working-directory: sub\n\
             \x20       env: { A: b }\n",
        );
        stub_run_scripts(&mut graph);
        let step = graph
            .nodes
            .iter()
            .find(|n| n.step.kind.as_ref() == RUN_KIND)
            .expect("a run node");
        let config = step.step.config.as_object().expect("an object config");
        assert_eq!(config.get("run"), Some(&Value::from("true")));
        assert_eq!(config.get("shell"), Some(&Value::from("sh")));
        assert!(!config.contains_key("shell_command"));
        assert!(!config.contains_key("env"));
        assert!(!config.contains_key("working_dir"));
    }

    #[test]
    fn host_scopes_containerize_by_label_and_container_jobs_keep_their_image() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 old:\n\
             \x20   runs-on: ubuntu-22.04\n\
             \x20   steps: [{run: echo}]\n\
             \x20 boxed:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   container: alpine:3.20\n\
             \x20   steps: [{run: echo}]\n",
        );
        containerize(&mut graph, None, |req| battery_image(req).to_string());
        let images: Vec<String> = graph
            .scopes
            .iter()
            .filter_map(|s| match &s.runtime.target {
                RuntimeTarget::Container { image, .. } => Some(image.to_string()),
                RuntimeTarget::HostProcess => None,
            })
            .collect();
        assert!(
            images.contains(&RUNNER_IMAGE_2204.to_string()),
            "{images:?}"
        );
        assert!(images.contains(&"alpine:3.20".to_string()), "{images:?}");
        assert!(
            !graph
                .scopes
                .iter()
                .any(|s| matches!(s.runtime.target, RuntimeTarget::HostProcess))
        );
    }

    #[test]
    fn expansions_cap_to_the_first_leg() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 build:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   strategy: { matrix: { v: [1, 2, 3] } }\n\
             \x20   steps: [{run: echo}]\n",
        );
        cap_expansions(&mut graph);
        let items = graph
            .nodes
            .iter()
            .find_map(|n| {
                let Expansion::ForEach { items, .. } = n.expand.as_ref()?;
                Some(*items)
            })
            .expect("the matrix expansion");
        // `[old[0]]`: a one-element array around an index into the old items.
        let Some(Expr::Array(elements)) = graph.exprs.get(items) else {
            panic!("capped items are an array literal");
        };
        assert_eq!(elements.len(), 1);
        assert!(matches!(
            graph.exprs.get(elements[0]),
            Some(Expr::Index(..))
        ));
    }

    #[test]
    fn docker_driving_graphs_pick_the_privileged_dind_runner() {
        let mut graph = lower(
            "on: push\n\
             jobs:\n\
             \x20 build:\n\
             \x20   runs-on: ubuntu-latest\n\
             \x20   steps: [{run: echo}]\n",
        );
        assert!(!needs_docker(&graph), "a stubbed script drives nothing");
        let node = graph
            .nodes
            .iter_mut()
            .find(|n| n.step.kind.as_ref() == RUN_KIND)
            .expect("a run node");
        node.step.kind = ACTION_KIND.into();
        node.step.config = serde_json::json!({
            "action": serde_json::to_value(pinned("docker/build-push-action@v6")).unwrap(),
            "inputs": {},
        });
        assert!(needs_docker(&graph));
        // The sweep's selection: dind where 24.04 would have been picked.
        containerize(&mut graph, None, |req| {
            let image = battery_image(req);
            if image == RUNNER_IMAGE_2404 {
                RUNNER_IMAGE_2404_DIND.to_string()
            } else {
                image.to_string()
            }
        });
        privilege(&mut graph, RUNNER_IMAGE_2404_DIND);
        let options: Vec<_> = graph
            .scopes
            .iter()
            .filter_map(|s| match &s.runtime.target {
                RuntimeTarget::Container { image, options, .. }
                    if image == RUNNER_IMAGE_2404_DIND =>
                {
                    Some(options.clone())
                }
                _ => None,
            })
            .collect();
        assert!(!options.is_empty(), "a dind scope exists");
        assert!(
            options
                .iter()
                .all(|o| o.iter().any(|flag| flag == "--privileged")),
            "{options:?}"
        );
    }

    fn pinned(reference: &str) -> ActionLocation {
        ActionLocation::Pinned(PinnedAction {
            reference: ActionRef::parse(reference).expect("a valid reference"),
            sha:       "0123456789012345678901234567890123456789".into(),
        })
    }

    #[test]
    fn identities_read_the_reference_and_the_cross_run_input() {
        let plain = serde_json::json!({
            "action": serde_json::to_value(pinned("actions/download-artifact@v4")).unwrap(),
            "inputs": {"name": "dist"},
        });
        assert_eq!(action_identity(&plain), StepIdentity::Action {
            bare:      "actions/download-artifact".to_string(),
            cross_run: false,
        });
        let cross = serde_json::json!({
            "action": serde_json::to_value(pinned("actions/download-artifact@v4")).unwrap(),
            "inputs": {"run-id": "123"},
        });
        let identity = action_identity(&cross);
        assert_eq!(
            expected_reason(&identity, "network", false),
            Some("cross-run artifact download (REST API)".to_string())
        );
    }

    #[test]
    fn classification_separates_gap_from_server_coupled() {
        let setup = StepIdentity::Action {
            bare:      "actions/setup-python".to_string(),
            cross_run: false,
        };
        assert_eq!(expected_reason(&setup, "exit_status:1", false), None);
        assert_eq!(
            expected_reason(&setup, "secret_unavailable", false),
            Some("needs a repository secret".to_string())
        );
        let oidc = StepIdentity::Action {
            bare:      "actions/attest-build-provenance".to_string(),
            cross_run: false,
        };
        assert_eq!(
            expected_reason(&oidc, "exit_status:1", false),
            Some("OIDC attestation".to_string())
        );
        // A mutating action is expected whatever the credential: the sweep
        // must never see its write succeed.
        let mutating = StepIdentity::Action {
            bare:      "dessant/lock-threads".to_string(),
            cross_run: false,
        };
        assert_eq!(
            expected_reason(&mutating, "exit_status:1", true),
            Some("mutates issues over the API".to_string())
        );
        // The real checkout cannot fetch without a credential; that is the
        // sweep's constraint, not a runtime-tier gap.
        let checkout = StepIdentity::Action {
            bare:      "actions/checkout".to_string(),
            cross_run: false,
        };
        assert!(expected_reason(&checkout, "exit_status:1", false).is_some());
        assert!(
            expected_reason(&checkout, "exit_status:1", true).is_none(),
            "with a real token, a checkout failure is a gap again"
        );
    }

    #[test]
    fn record_names_strip_the_clone_suffix() {
        let mut identities = BTreeMap::new();
        identities.insert("build/step".to_string(), StepIdentity::Run);
        assert!(identity_of(&identities, "build/step").is_some());
        assert!(identity_of(&identities, "build/step#2").is_some());
        assert!(identity_of(&identities, "build#0/step#11").is_some());
        assert!(identity_of(&identities, "build/other").is_none());
    }

    /// Every path segment loses its clone suffix — a first-`#` cut would
    /// truncate a multi-segment clone name to its head segment.
    #[test]
    fn clone_base_strips_each_segment() {
        assert_eq!(clone_base("build/step"), "build/step");
        assert_eq!(clone_base("build/step#2"), "build/step");
        assert_eq!(clone_base("build#0/step#11"), "build/step");
        // A non-numeric `#` is part of the name, not a clone suffix.
        assert_eq!(clone_base("build/step#done"), "build/step#done");
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn stubbed_build_uploads_classify_from_the_log() {
        let upload = StepIdentity::Action {
            bare:      "actions/upload-artifact".to_string(),
            cross_run: false,
        };
        let miss = vec![
            "Error: No files were found with the provided path: dist/. No artifacts will be \
             uploaded."
                .to_string(),
        ];
        assert!(expected_from_log(&upload, &miss).is_some());
        // Any other upload failure stays a gap; so does the same log elsewhere.
        assert!(expected_from_log(&upload, &["ECONNREFUSED".to_string()]).is_none());
        // github-script: an unhandled throw inside the inline script is the
        // script's own business (locally, usually the empty event).
        let script = StepIdentity::Action {
            bare:      "actions/github-script".to_string(),
            cross_run: false,
        };
        let threw = vec!["Error: Unhandled error: SyntaxError: Unexpected token ';'".to_string()];
        assert!(expected_from_log(&script, &threw).is_some());
        assert!(expected_from_log(&script, &["exit 1".to_string()]).is_none());
        let other = StepIdentity::Action {
            bare:      "actions/setup-node".to_string(),
            cross_run: false,
        };
        assert!(expected_from_log(&other, &miss).is_none());
    }

    /// The scorecard action fails on the result-signing service rejecting the
    /// sweep's PAT — the one line that names it can sit far above the results
    /// dump the step prints last. Keyed on that line: a scorecard log without
    /// it — the scan output alone, or any other failure — stays a gap.
    #[test]
    fn scorecard_signing_rejection_classifies_and_the_scan_alone_does_not() {
        let scorecard = StepIdentity::DockerAction("ghcr.io/ossf/scorecard-action:v2.4.4".into());
        let signing = vec![
            "2026/08/29 18:06:29 error SigningNew: invalid token: not a default GITHUB_TOKEN"
                .to_string(),
            r#"{"date":"2026-08-29","repo":{"name":"github.com/prometheus/prometheus"}}"#
                .to_string(),
        ];
        assert_eq!(
            expected_from_log(&scorecard, &signing),
            Some("signs results with the Actions-issued GITHUB_TOKEN".to_string())
        );
        // A success-shaped log — the results JSON with no signing rejection —
        // classifies nothing; neither does the signature under another step.
        let results_only = vec![
            r#"{"date":"2026-08-29","scorecard":{"version":"v5.5.0"},"score":8.1}"#.to_string(),
        ];
        assert!(expected_from_log(&scorecard, &results_only).is_none());
        let other = StepIdentity::DockerAction("ghcr.io/other/tool:v1".to_string());
        assert!(expected_from_log(&other, &signing).is_none());
    }

    /// paths-filter forced into git-diff mode by the synthesized event, over a
    /// job that checked nothing out: expected. The same action failing without
    /// the no-repository fatal — a real git-mode diff gone wrong — stays a gap.
    #[test]
    fn paths_filter_without_a_repository_classifies_and_git_mode_failures_do_not() {
        let filter = StepIdentity::Action {
            bare:      "dorny/paths-filter".to_string(),
            cross_run: false,
        };
        let no_repo = vec![
            "fatal: not a git repository (or any of the parent directories): .git".to_string(),
            "Error: The process 'git rev-parse --abbrev-ref HEAD' failed with exit code 128"
                .to_string(),
        ];
        assert_eq!(
            expected_from_log(&filter, &no_repo),
            Some("reads the triggering pull request (no local event)".to_string())
        );
        let diff_broke =
            vec!["Error: The process 'git diff --name-only' failed with exit code 129".to_string()];
        assert!(expected_from_log(&filter, &diff_broke).is_none());
        // A stray no-repository mention with a different terminal failure is
        // not the cause.
        let stray = vec![
            "fatal: not a git repository (or any of the parent directories): .git".to_string(),
            "Error: Unable to read the filter configuration".to_string(),
        ];
        assert!(expected_from_log(&filter, &stray).is_none());
        let other = StepIdentity::Action {
            bare:      "actions/setup-node".to_string(),
            cross_run: false,
        };
        assert!(expected_from_log(&other, &no_repo).is_none());
    }

    /// codeql analyze is expected only when the failure IS the own-run API
    /// lookup; the cli/cli database-finalize fatal — same action, different
    /// cause — must stay a gap.
    #[test]
    fn codeql_analyze_own_run_lookup_classifies_and_finalize_fatals_do_not() {
        let analyze = StepIdentity::Action {
            bare:      "github/codeql-action/analyze".to_string(),
            cross_run: false,
        };
        let not_found = vec![
            "Error: Not Found - https://docs.github.com/rest/actions/workflow-runs#get-a-workflow-run"
                .to_string(),
        ];
        assert_eq!(
            expected_from_log(&analyze, &not_found),
            Some("reads its own workflow run (no server-side run exists)".to_string())
        );
        // The cli/cli shape: the own-run 404 appears in passing — as telemetry
        // noise and as an earlier error — then the step dies on a
        // database-finalize fatal. The last error names the real cause, and
        // the incidental mentions must not classify it.
        let finalize = vec![
            "Warning: Failed to gather information for telemetry: Not Found - \
             https://docs.github.com/rest/actions/workflow-runs#get-a-workflow-run"
                .to_string(),
            "Error: Not Found - https://docs.github.com/rest/actions/workflow-runs#get-a-workflow-run"
                .to_string(),
            "Error: Encountered a fatal error while running \"codeql database finalize \
             --finalize-dataset\""
                .to_string(),
        ];
        assert!(expected_from_log(&analyze, &finalize).is_none());
        // The same 404 under a different action names a different problem.
        let other = StepIdentity::Action {
            bare:      "github/codeql-action/init".to_string(),
            cross_run: false,
        };
        assert!(expected_from_log(&other, &not_found).is_none());
    }

    /// The setup-node shape that made a whole gap class undiagnosable: the
    /// real error, then deprecation noise the report used to print instead.
    #[test]
    fn the_error_line_outranks_trailing_noise() {
        let tail = vec![
            "Error: Cache service responded with 503".to_string(),
            "(node:42) [DEP0005] DeprecationWarning: Buffer() is deprecated".to_string(),
            "(Use `node --trace-deprecation ...` to show where the warning was created)"
                .to_string(),
        ];
        assert_eq!(
            error_line(&tail).unwrap(),
            "Error: Cache service responded with 503"
        );
        // The marker is a prefix, not a substring: a line that merely mentions
        // an error is not the error.
        assert!(error_line(&["the previous error repeated".to_string()]).is_none());
        assert!(error_line(&[]).is_none());
    }
}
