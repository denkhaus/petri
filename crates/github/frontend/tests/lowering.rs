//! Handoff §7 tests 3–6, the pure half: what lowering produces, without running it.
//! The behavioural half — the same constructs through the engine on real processes —
//! lives in `crates/github/acceptance/tests/gha_e2e.rs`.

mod support;

use frontend::Severity;
use serde_json::json;
use support::*;

#[test]
fn composite_depth_cap_is_a_diagnostic() {
    let recursive = r#"
runs:
  using: composite
  steps:
    - uses: ./.github/actions/loop
"#;
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: ./.github/actions/loop
"#;
    let files = files(&[(".github/actions/loop/action.yml", recursive)]);
    let diags = diagnostics_with(text, &files);
    let depth = diags
        .iter()
        .find(|d| d.code == "gha.composite_depth")
        .expect("depth cap reported");
    assert_eq!(depth.severity, Severity::Error);
}

#[test]
fn custom_shells_lower_to_a_template_and_windows_shells_stay_rejected() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo login
        shell: bash -leo pipefail {0}
      - run: print("hi")
        shell: python
      - run: Get-Location
        shell: pwsh
      - run: echo env
        shell: /usr/bin/env bash {0}
"#;
    let graph = lower_ok(text);
    let shell_command = |name: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .step
            .config["shell_command"]
            .clone()
    };
    assert_eq!(shell_command("j/step-1"), json!("bash -leo pipefail {0}"));
    assert_eq!(shell_command("j/step-2"), json!("python {0}"));
    assert_eq!(
        shell_command("j/step-3"),
        json!("pwsh -command \". '{0}'\"")
    );
    assert_eq!(shell_command("j/step-4"), json!("/usr/bin/env bash {0}"));

    let diags = diagnostics(
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        shell: cmd\n",
    );
    assert!(
        diags.iter().any(|d| d.code == "unsupported.shell.cmd"),
        "{diags:?}"
    );
}

// ── §7 test 6: secrets ────────────────────────────────────────────────────

#[test]
fn secrets_lower_to_refs_in_env_and_are_errors_elsewhere() {
    let text = r#"
on: push
env:
  TOP: ${{ secrets.TOP_TOKEN }}
jobs:
  j:
    runs-on: ubuntu-latest
    env:
      JOB: ${{ secrets.JOB_TOKEN }}
    steps:
      - env:
          STEP: ${{ secrets.STEP_TOKEN }}
        run: echo "$TOP $JOB $STEP"
"#;
    let graph = lower_ok(text);
    let step = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    let env = &step.step.config["env"];
    assert_eq!(
        env["TOP"],
        json!({"$secret": "TOP_TOKEN"}),
        "workflow-level secrets push down into the step"
    );
    assert_eq!(env["JOB"], json!({"$secret": "JOB_TOKEN"}));
    assert_eq!(env["STEP"], json!({"$secret": "STEP_TOKEN"}));
    // No secret value, and no secret *reference* in scope env either.
    let encoded = serde_json::to_string(&graph).unwrap();
    assert!(!encoded.contains("s3cret"));
    for scope in &graph.scopes {
        assert!(
            !scope.env.contains_key("TOP"),
            "secret refs do not live in scope env"
        );
    }

    for bad in ["    if: ${{ secrets.X == 'y' }}", "    if: secrets.X"] {
        let text = format!(
            r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
{bad}
    steps:
      - run: echo hi
"#
        );
        let diags = diagnostics(&text);
        assert!(
            diags
                .iter()
                .any(|d| d.code == "unsupported.secrets.expression"),
            "{bad}: {diags:?}"
        );
    }
    // Inside a larger string in step config it lowers: the step resolves the
    // secret's sentinel at spawn, and the log never sees the value.
    let diags = diagnostics(
        r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - env:
          URL: https://user:${{ secrets.PW }}@host
        run: echo hi
"#,
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code == "unsupported.secrets.expression"),
        "{diags:?}"
    );
}

/// `concurrency:` lowers — the group is cross-run mutual exclusion, meaningless
/// in a single local run — but never silently: a warning names what was ignored,
/// at the workflow level and the job level alike.
#[test]
fn concurrency_lowers_with_a_warning() {
    let text = "concurrency: group-a\non: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    concurrency: { group: g, cancel-in-progress: true }\n    steps:\n      - run: echo\n";
    let graph = lower_ok(text);
    assert!(!graph.nodes.is_empty());
    let diags = diagnostics(text);
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.code == "ignored.concurrency")
            .count(),
        2,
        "{diags:?}"
    );
}

/// `environment:` lowers — approvals, protection rules and environment-scoped
/// secrets are GitHub server features a local run cannot enforce — but never
/// silently, and the target survives on the job's `start` node so the graph says
/// what the job would have deployed to. Expression-valued names stay as written:
/// an ignored field is never evaluated.
#[test]
fn environment_lowers_with_a_warning_and_the_target_is_preserved() {
    let text = r#"
on: push
jobs:
  simple:
    runs-on: ubuntu-latest
    environment: production
    steps:
      - run: echo
  full:
    runs-on: ubuntu-latest
    environment:
      name: ${{ inputs.environment }}
      url: ${{ steps.deploy.outputs.url }}
      deployment: true
    steps:
      - run: echo
"#;
    let graph = lower_ok(text);
    let meta = |name: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .meta
            .clone()
    };
    assert_eq!(
        meta("simple/start"),
        json!({ "environment": { "name": "production" } })
    );
    assert_eq!(
        meta("full/start"),
        json!({ "environment": {
            "name": "${{ inputs.environment }}",
            "url": "${{ steps.deploy.outputs.url }}",
            "deployment": "true",
        } })
    );
    let diags = diagnostics(text);
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| d.code == "ignored.environment")
        .collect();
    assert_eq!(warnings.len(), 2, "{diags:?}");
    for w in warnings {
        assert_eq!(w.severity, Severity::Warning);
        assert!(
            w.message.contains("protection rules") && w.message.contains("secrets"),
            "the warning names what a local run does not have: {}",
            w.message
        );
    }

    // Malformed values still fail: a list, a mapping with no name, a stray key.
    for (bad, code) in [
        ("environment: [a, b]", "gha.bad_environment"),
        ("environment: { url: https://x }", "gha.bad_environment"),
        (
            "environment: { name: p, on-failure: q }",
            "yaml.unknown_key",
        ),
    ] {
        let text = format!(
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    {bad}\n    steps:\n      - run: echo\n"
        );
        let diags = diagnostics(&text);
        assert!(
            diags
                .iter()
                .any(|d| d.code == code && d.severity == Severity::Error),
            "{bad}: {diags:?}"
        );
    }
}

/// `runs-on: ${{ matrix.os }}` resolves at lowering, once per leg, as GitHub
/// resolves it at queue time — the legs come from the same combinators the
/// engine expands with. Each leg's labels are preserved on the job's `start`
/// node, and their union becomes the scope's placement requirements.
#[test]
fn expression_runs_on_resolves_per_matrix_leg() {
    let text = r#"
on: push
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-latest, ubuntu-24.04]
        exclude:
          - os: ubuntu-24.04
        include:
          - os: ubuntu-22.04
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo
"#;
    let graph = lower_ok(text);
    let start = graph
        .nodes
        .iter()
        .find(|n| n.name == "test/start")
        .expect("test/start");
    assert_eq!(
        start.meta["runs_on"],
        json!([
            { "leg": { "os": "ubuntu-latest" }, "labels": ["ubuntu-latest"] },
            { "leg": { "os": "ubuntu-22.04" }, "labels": ["ubuntu-22.04"] },
        ]),
        "each leg keeps its own result, through exclude and include"
    );
    let scope = graph.scope(start.scope).unwrap();
    assert_eq!(
        scope.runtime.requirements,
        ["ubuntu-latest", "ubuntu-22.04"]
    );
}

/// The shapes the handoff names: a label list from `fromJSON`, a template
/// around a matrix value, and a nested matrix object property.
#[test]
fn expression_runs_on_supports_lists_templates_and_nested_values() {
    let text = r#"
on: push
jobs:
  list:
    runs-on: ${{ fromJSON('["ubuntu-latest"]') }}
    steps:
      - run: echo
  template:
    strategy:
      matrix:
        os: [ubuntu]
    runs-on: ${{ matrix.os }}-latest
    steps:
      - run: echo
  nested:
    strategy:
      matrix:
        runner:
          - os: ubuntu-24.04
    runs-on: ${{ matrix.runner.os }}
    steps:
      - run: echo
"#;
    let graph = lower_ok(text);
    let requirements = |job: &str| {
        let start = graph
            .nodes
            .iter()
            .find(|n| n.name == format!("{job}/start"))
            .unwrap();
        graph
            .scope(start.scope)
            .unwrap()
            .runtime
            .requirements
            .clone()
    };
    assert_eq!(requirements("list"), ["ubuntu-latest"]);
    assert_eq!(requirements("template"), ["ubuntu-latest"]);
    assert_eq!(requirements("nested"), ["ubuntu-24.04"]);
}

/// A leg that resolves to an unsupported runner rejects with that runner's own
/// code, named per leg — and does not stop the supported legs from resolving.
#[test]
fn unsupported_legs_are_named_and_do_not_corrupt_supported_ones() {
    let text = r#"
on: push
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo
"#;
    let diags = diagnostics(text);
    let windows = diags
        .iter()
        .find(|d| d.code == "unsupported.runs_on.windows")
        .expect("the windows leg rejects with the windows code");
    assert!(
        windows.message.contains("windows-latest") && windows.message.contains("matrix leg"),
        "{}",
        windows.message
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code == "unsupported.runs_on.expression"),
        "resolution succeeded; the rejection is about the runner: {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| d.message.contains("ubuntu-latest")),
        "the supported leg is not the problem: {diags:?}"
    );
}

/// What cannot be resolved before the run stays a specific rejection: a
/// run-time context, a dynamic matrix, a value that is not a label.
#[test]
fn unresolvable_runs_on_expressions_stay_rejected() {
    for (bad, wants) in [
        (
            "    runs-on: ${{ needs.plan.outputs.runner }}\n",
            "no value before the run",
        ),
        (
            "    strategy: { matrix: { os: \"${{ fromJSON(needs.plan.outputs.legs) }}\" } }\n    runs-on: ${{ matrix.os }}\n",
            "not static",
        ),
        (
            "    strategy: { matrix: { os: [ubuntu-latest] } }\n    runs-on: ${{ matrix.missing }}\n",
            "not a label",
        ),
    ] {
        let text = format!("on: push\njobs:\n  j:\n{bad}    steps:\n      - run: echo\n");
        let diags = diagnostics(&text);
        let d = diags
            .iter()
            .find(|d| d.code == "unsupported.runs_on.expression")
            .unwrap_or_else(|| panic!("{bad}: {diags:?}"));
        assert!(d.message.contains(wants), "{bad}: {}", d.message);
    }
}

/// The runner map: configuration adds third-party and self-hosted labels that
/// name Linux environments this machine can stand in for. Static and
/// expression-derived labels answer to the same map; all of a job's labels must
/// resolve; Windows and macOS stay specific errors whatever the map says.
#[test]
fn the_runner_map_places_configured_labels() {
    use frontend::NoFiles;
    use frontend_gha::{RunnerMap, load_configured};

    let text = r#"
on: push
jobs:
  hosted:
    runs-on: depot-ubuntu-24.04-8
    steps:
      - run: echo
  self-hosted:
    runs-on: [self-hosted, linux, x64]
    steps:
      - run: echo
  derived:
    strategy:
      matrix:
        pool: [depot-ubuntu-24.04-8]
    runs-on: ${{ matrix.pool }}
    steps:
      - run: echo
"#;
    let configured = RunnerMap::builtin().allow_list("depot-ubuntu-24.04-8, self-hosted linux,x64");
    let lowered = load_configured(
        ".github/workflows/test.yml",
        text,
        &NoFiles,
        None,
        &configured,
    );
    assert!(
        lowered.graph.is_some(),
        "{:?}",
        lowered.diagnostics.into_vec()
    );

    // All labels must match: one unmapped label in the set is that label's own
    // explicit rejection.
    let partial = RunnerMap::builtin().allow_list("self-hosted linux");
    let text = "on: push\njobs:\n  j:\n    runs-on: [self-hosted, linux, x64]\n    steps:\n      - run: echo\n";
    let lowered = load_configured(".github/workflows/test.yml", text, &NoFiles, None, &partial);
    assert!(lowered.graph.is_none());
    let diags = lowered.diagnostics.into_vec();
    let unknown: Vec<&frontend::Diagnostic> = diags
        .iter()
        .filter(|d| d.code == "unsupported.runs_on.unknown")
        .collect();
    assert_eq!(unknown.len(), 1, "{diags:?}");
    assert!(unknown[0].message.contains("x64"), "{}", unknown[0].message);

    // Windows and macOS remain specific errors even when the map claims them.
    let contradiction = RunnerMap::builtin().allow_list("windows-large macos-pool");
    let text = "on: push\njobs:\n  w:\n    runs-on: windows-large\n    steps:\n      - run: echo\n  m:\n    runs-on: macos-pool\n    steps:\n      - run: echo\n";
    let lowered = load_configured(
        ".github/workflows/test.yml",
        text,
        &NoFiles,
        None,
        &contradiction,
    );
    let diags = lowered.diagnostics.into_vec();
    for code in ["unsupported.runs_on.windows", "unsupported.runs_on.macos"] {
        assert!(diags.iter().any(|d| d.code == code), "{code}: {diags:?}");
    }
}

/// A `github.*` guard resolves against the checkout's declared identity: the
/// origin remote's slug where one exists, the fallback branch everywhere else.
/// In the named checkout the guard honestly selects its hosted pool — which is
/// then the runner map's question, not a mystery expression.
#[test]
fn github_guards_resolve_against_the_checkout_identity() {
    let text = "on: push\njobs:\n  j:\n    runs-on: ${{ github.repository == 'astral-sh/ruff' && 'depot-fast-32' || 'ubuntu-latest' }}\n    steps:\n      - run: echo\n";
    // No checkout: the guard takes its fallback and lowers.
    let graph = lower_ok(text);
    let start = graph.nodes.iter().find(|n| n.name == "j/start").unwrap();
    assert_eq!(
        graph.scope(start.scope).unwrap().runtime.requirements,
        ["ubuntu-latest"]
    );

    // The named checkout: the guard selects the opaque pool, and the runner
    // map's rejection names it.
    let checkout = files(&[(
        ".git/config",
        "[remote \"origin\"]\n\turl = git@github.com:astral-sh/ruff.git\n",
    )]);
    let diags = diagnostics_with(text, &checkout);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "unsupported.runs_on.unknown" && d.message.contains("depot-fast-32")),
        "{diags:?}"
    );

    // A matrix axis guarded the same way is as static as a literal one.
    let matrix = r#"
on: push
jobs:
  j:
    strategy:
      matrix:
        big: ["${{ github.ref == 'refs/heads/main' }}"]
        os: [ubuntu-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo
"#;
    let graph = lower_ok(matrix);
    assert!(!graph.nodes.is_empty());
    assert!(diagnostics(matrix).is_empty(), "{:?}", diagnostics(matrix));
}

/// A label's own tokens decide its platform: Ubuntu/Linux pools place without
/// configuration, Windows and macOS pools are their platforms' rejections
/// wherever the token appears, and only genuinely opaque labels need the map.
#[test]
fn labels_place_by_their_own_tokens() {
    let text = r#"
on: push
jobs:
  depot:
    runs-on: depot-ubuntu-22.04-16
    steps:
      - run: echo
  xl:
    runs-on: [ubuntu-24.04-xl, ubuntu-26.04-arm]
    steps:
      - run: echo
"#;
    let graph = lower_ok(text);
    assert!(!graph.nodes.is_empty());
    assert!(diagnostics(text).is_empty(), "{:?}", diagnostics(text));

    for (label, code) in [
        ("namespace-profile-macos-15", "unsupported.runs_on.macos"),
        (
            "namespace-profile-windows-2022-x86-64-4",
            "unsupported.runs_on.windows",
        ),
        ("codspeed-macro", "unsupported.runs_on.unknown"),
    ] {
        let text = format!(
            "on: push\njobs:\n  j:\n    runs-on: {label}\n    steps:\n      - run: echo\n"
        );
        let diags = diagnostics(&text);
        assert!(diags.iter().any(|d| d.code == code), "{label}: {diags:?}");
    }
}

/// Windows and macOS runners are out of scope: the local executor emulates
/// Linux runners only.
#[test]
fn windows_and_macos_runners_are_rejected() {
    for (label, code) in [
        ("windows-latest", "unsupported.runs_on.windows"),
        ("macos-latest", "unsupported.runs_on.macos"),
        ("macos-14", "unsupported.runs_on.macos"),
    ] {
        let diags = diagnostics(&format!(
            "on: push\njobs:\n  j:\n    runs-on: {label}\n    steps:\n      - run: echo\n"
        ));
        assert!(diags.iter().any(|d| d.code == code), "{label}: {diags:?}");
    }
}

// ── The rejection set ─────────────────────────────────────────────────────

#[test]
fn the_rejection_set_is_loud_and_specific() {
    let cases: &[(&str, &str)] = &[
        (
            "on: push\njobs:\n  j:\n    runs-on: windows-latest\n    steps:\n      - run: echo\n",
            "unsupported.runs_on.windows",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: [self-hosted, gpu]\n    steps:\n      - run: echo\n",
            "unsupported.runs_on.unknown",
        ),
        (
            // `matrix`, `inputs` and the checkout identity resolve at lowering;
            // `needs` never can.
            "on: push\njobs:\n  j:\n    runs-on: ${{ needs.plan.outputs.runner }}\n    steps:\n      - run: echo\n",
            "unsupported.runs_on.expression",
        ),
        (
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
            "unsupported.action.remote",
        ),
        (
            // `pwsh`, `python` and `{0}` templates lower now; Windows shells stay.
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        shell: cmd\n",
            "unsupported.shell.cmd",
        ),
        (
            // A step's `if:` may call hashFiles (the step resolves it); a job's
            // `if:` is evaluated by the engine, with no workspace — as on
            // GitHub, whose job conditions have no hashFiles either.
            "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    if: hashFiles('**/lock') != ''\n    steps:\n      - run: echo\n",
            "unsupported.expression.hashFiles",
        ),
        (
            // A remote workflow call with no action source: the same rejection
            // remote actions get.
            "on: push\njobs:\n  j:\n    uses: org/repo/.github/workflows/x.yml@main\n",
            "unsupported.action.remote",
        ),
    ];
    for (text, code) in cases {
        let diags = diagnostics(text);
        let found = diags.iter().find(|d| d.code == *code);
        assert!(
            found.is_some(),
            "expected {code} for:\n{text}\ngot {diags:#?}"
        );
        let d = found.unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.hint.is_some(), "{code} needs a hint");
        assert!(d.span.line > 0, "{code} needs a span");
    }
    // The remote-action message names the action, for the histogram.
    let diags = diagnostics(
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
    );
    let d = diags
        .iter()
        .find(|d| d.code == "unsupported.action.remote")
        .unwrap();
    assert!(d.message.contains("actions/checkout@v4"), "{}", d.message);
}

#[test]
fn a_clean_workflow_lowers_to_the_documented_shape() {
    let text = r#"
name: ci
on: push
env:
  GLOBAL: g
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      LOCAL: l
    outputs:
      artifact: ${{ steps.compile.outputs.artifact }}
    steps:
      - id: compile
        run: echo "artifact=app.tar" >> "$GITHUB_OUTPUT"
        timeout-minutes: 5
  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo "testing ${{ needs.build.outputs.artifact }}"
  lint:
    needs: build
    runs-on: ubuntu-latest
    container: alpine:3.20
    steps:
      - run: echo lint
"#;
    let graph = lower_ok(text);
    let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "build/start",
            "build/done",
            "test/start",
            "test/done",
            "lint/start",
            "lint/done",
            "build/compile",
            "test/step-1",
            "lint/step-1"
        ]
    );
    // k dependents → k single-arm groups on `done`.
    let build_done = graph.nodes.iter().find(|n| n.name == "build/done").unwrap();
    assert_eq!(build_done.routing.groups.len(), 2);
    assert!(build_done.routing.groups.iter().all(|g| g.arms.len() == 1));
    // needs → All join on the dependent's start.
    let test_start = graph.nodes.iter().find(|n| n.name == "test/start").unwrap();
    assert_eq!(test_start.join, ir::JoinPolicy::All);
    // timeout-minutes → budget.
    let compile = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/compile")
        .unwrap();
    assert_eq!(compile.budget.timeout.as_secs(), 300);
    // container → Docker scope; runs-on → requirements.
    let lint_scope = graph
        .scope(
            graph
                .nodes
                .iter()
                .find(|n| n.name == "lint/step-1")
                .unwrap()
                .scope,
        )
        .unwrap();
    assert!(
        matches!(&lint_scope.runtime.target, ir::RuntimeTarget::Container { image, .. } if image == "alpine:3.20")
    );
    assert_eq!(
        lint_scope.runtime.requirements,
        vec![smol_str::SmolStr::new("ubuntu-latest")]
    );
    // Env layering.
    let build_scope = graph.scope(compile.scope).unwrap();
    assert!(build_scope.env.contains_key("GLOBAL"));
    assert!(build_scope.env.contains_key("LOCAL"));
    assert!(build_scope.env.contains_key("GITHUB_SHA"));
    // GHA exercises the degenerate subset.
    assert!(!graph.edges().any(|e| e.back));
    assert!(graph.nodes.iter().all(|n| n.join == ir::JoinPolicy::All));
    ir::validate(&graph).expect("valid");
}

// ── Cancellation flags and gates ──────────────────────────────────────────

/// After a polite cancel, conditions decide: every node the lowering emits
/// carries `run_on_cancel` (spec §5), fires, and its gate or precondition lands
/// on GitHub's truth table — no condition text is sniffed for admission. The one
/// exception is a matrix `start`, which carries the expansion: a cancelled scope
/// never splices, so a leg not yet started stays cancelled, as GitHub cancels a
/// queued `fail-fast` leg. Step nodes carry no engine precondition at all; their
/// condition is the `gate` in their config.
#[test]
fn run_on_cancel_lands_exactly_where_github_keeps_going() {
    let action = r#"
runs:
  using: composite
  steps:
    - shell: bash
      run: echo one
    - shell: bash
      run: echo two
"#;
    let text = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo build
      - id: tidy
        if: always()
        run: echo tidy
      - id: onfail
        if: failure()
        run: echo onfail
      - id: sweep
        if: cancelled()
        uses: ./.github/actions/sweeper
      - id: plain
        uses: ./.github/actions/sweeper
  cleanup:
    needs: build
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo cleanup
  legs:
    needs: build
    runs-on: ubuntu-latest
    strategy:
      matrix:
        n: [1, 2]
    steps:
      - run: echo ${{ matrix.n }}
"#;
    let files = files(&[(".github/actions/sweeper/action.yml", action)]);
    let graph = lower_ok_with(text, &files);

    let unflagged: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| !n.run_on_cancel)
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(
        unflagged,
        vec!["legs/start"],
        "only the matrix expansion head stays unflagged"
    );

    for node in &graph.nodes {
        let structural = node.name.ends_with("/start") || node.name.ends_with("/done");
        if structural {
            assert!(
                node.step.config.get("gate").is_none(),
                "{}: start and done carry no gate",
                node.name
            );
            continue;
        }
        assert!(
            node.precondition.is_none(),
            "{}: step nodes carry no engine precondition",
            node.name
        );
        assert!(
            node.step.config.get("gate").is_some() && node.step.config.get("cancelled").is_some(),
            "{}: the condition is the config gate",
            node.name
        );
    }
    // Job-level `if:` stays an engine precondition on `start`.
    let cleanup_start = graph
        .nodes
        .iter()
        .find(|n| n.name == "cleanup/start")
        .unwrap();
    assert!(cleanup_start.precondition.is_some());
    // `start` records whether it fired under a cancel, for the steps' gates.
    assert!(
        cleanup_start.step.config["cancelled"]
            .get("$expr")
            .is_some(),
        "start records the scope_cancelled static in its output"
    );
}

// ── Step-condition gates ──────────────────────────────────────────────────

/// What a step's `if:` lowers to, by shape: nothing only the step can resolve
/// collapses to a single engine-expression leaf; `env.NAME` and literal-pattern
/// `hashFiles` become step-resolved leaves under the GitHub operators.
#[test]
fn step_conditions_lower_to_gates() {
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - id: plain
        if: github.ref == 'refs/heads/main'
        run: echo plain
      - id: envy
        if: env.DEPLOY == '1'
        run: echo envy
      - id: hashy
        if: hashFiles('**/Cargo.lock') != ''
        run: echo hashy
      - id: bare
        run: echo bare
"#;
    let graph = lower_ok(text);
    let gate = |name: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .step
            .config["gate"]
            .clone()
    };

    // The common case: one `$expr` leaf holding started && success && condition.
    for name in ["j/plain", "j/bare"] {
        let g = gate(name);
        assert!(
            g["lit"].get("$expr").is_some(),
            "{name}: a fully engine-evaluable condition is one leaf: {g}"
        );
    }

    // `env.DEPLOY == '1'`: the comparison is the step's, with an `$env` leaf
    // carrying the engine's scope-env view as its fallback.
    let envy = gate("j/envy");
    assert_eq!(envy["op"], json!("&&"));
    let cmp = envy["args"].as_array().unwrap().last().unwrap();
    assert_eq!(cmp["op"], json!("=="));
    assert_eq!(cmp["args"][0]["$env"], json!("DEPLOY"));
    assert!(cmp["args"][0].get("or").is_some(), "{envy}");
    assert_eq!(cmp["args"][1], json!({"lit": "1"}));

    // `hashFiles(...) != ''`: the pattern rides as a sentinel string literal the
    // step resolves against the workspace before comparing.
    let hashy = gate("j/hashy");
    let cmp = hashy["args"].as_array().unwrap().last().unwrap();
    assert_eq!(cmp["op"], json!("!="));
    let sentinel = cmp["args"][0]["lit"].as_str().unwrap();
    assert!(
        frontend_gha::exprs::has_hashfiles_sentinel(sentinel),
        "{sentinel:?}"
    );
    // The steps' `cancelled` bit is the engine's scope_cancelled static.
    let bare = graph.nodes.iter().find(|n| n.name == "j/bare").unwrap();
    assert!(bare.step.config["cancelled"].get("$expr").is_some());
    ir::validate(&graph).expect("valid");
}

/// The docs' context-availability matrix, encoded (checked 2026-08-27): a step
/// `if:` sees `env` and `hashFiles` (the step resolves them); a job `if:` sees
/// neither; secrets are absent from conditions at every level; a job output
/// that would carry a secret is dropped with a warning, as GitHub drops it.
#[test]
fn conditions_follow_the_context_availability_matrix() {
    // Step `if:` — env and hashFiles lower (no diagnostics).
    let ok = diagnostics(
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        if: env.X == '1' && hashFiles('**/lock') != ''\n",
    );
    assert!(ok.iter().all(|d| !d.is_error()), "{ok:?}");

    // Job `if:` — hashFiles has no workspace to read.
    let job_hash = diagnostics(
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    if: hashFiles('**/lock') != ''\n    steps:\n      - run: echo\n",
    );
    assert!(
        job_hash
            .iter()
            .any(|d| d.code == "unsupported.expression.hashFiles"),
        "{job_hash:?}"
    );

    // Secrets stay out of conditions at both levels, under operators included.
    for bad in [
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    if: secrets.X == 'y'\n    steps:\n      - run: echo\n",
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        if: secrets.X == 'y'\n",
        "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        if: env.A == '1' && secrets.X == 'y'\n",
    ] {
        let diags = diagnostics(bad);
        assert!(
            diags
                .iter()
                .any(|d| d.code == "unsupported.secrets.expression"),
            "{bad}: {diags:?}"
        );
    }

    // A job output that is a secret: dropped, with a warning naming it.
    let text = "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    outputs:\n      token: ${{ secrets.T }}\n    steps:\n      - run: echo\n";
    let diags = diagnostics(text);
    let warn = diags
        .iter()
        .find(|d| d.code == "ignored.secret_output")
        .expect("the drop is loud");
    assert!(!warn.is_error());
    let graph = lower_ok(text);
    assert!(
        !serde_json::to_string(&graph)
            .unwrap()
            .contains("petri-secret"),
        "the dropped output leaves no secret reference behind"
    );
}

/// `cancelled()` ORs in the engine's `scope_cancelled` static, at step and job
/// level, so a cancel no step record can show — between steps, before a job
/// starts, a `fail_fast` scope cancel — still reads as cancelled.
#[test]
fn cancelled_lowers_with_scope_cancelled() {
    let text = r#"
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: echo a
  b:
    needs: a
    if: cancelled()
    runs-on: ubuntu-latest
    steps:
      - if: cancelled()
        run: echo b
"#;
    let graph = lower_ok(text);
    let printed = frontend::print_graph(&graph);
    assert!(
        printed.contains("scope_cancelled"),
        "the static appears in the lowered expressions: {printed}"
    );
}
