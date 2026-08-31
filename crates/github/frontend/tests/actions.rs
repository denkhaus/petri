//! `uses: owner/repo@ref` lowering: a JavaScript action becomes `github/action`
//! nodes — main where the step is, `pre` before every step, `post` after the
//! last in reverse — pinned to the commit the source resolved, with inputs and
//! `github.token` lowered where the step is.

use frontend::NoFiles;
use frontend_gha::action::MapActionSource;
use frontend_gha::{ACTION_KIND, RUN_KIND, STATE_OUTPUT_KEY, load, load_with};
use serde_json::json;

const CHECKOUT: &str = r"
name: Checkout
inputs:
  repository:
    default: ${{ github.repository }}
  token:
    default: ${{ github.token }}
  fetch-depth:
    default: 1
runs:
  using: node20
  main: dist/index.js
  post: dist/cleanup.js
";

const WITH_PRE: &str = r"
name: Pre and post
runs:
  using: node24
  main: main.js
  pre: pre.js
  pre-if: runner.os == 'Linux'
  post: post.js
  post-if: success()
";

const TEST_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[expect(
    clippy::print_stderr,
    reason = "a test binary has no log sink; stderr carries the lowering diagnostics that \
              explain a failed assertion below"
)]
fn lower(text: &str, source: &MapActionSource) -> frontend::Lowered {
    let lowered = load_with(".github/workflows/ci.yml", text, &NoFiles, Some(source));
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

/// The step chain in creation order — every node but a job's `start` and
/// `done`, which the shell pass creates before any step.
fn chain(graph: &ir::Graph) -> Vec<&str> {
    graph
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .filter(|n| !n.ends_with("/start") && !n.ends_with("/done"))
        .collect()
}

#[test]
fn a_node_action_lowers_to_a_main_node_and_a_trailing_post_node() {
    let source = MapActionSource::new().with("octo/tool@v4", TEST_SHA, CHECKOUT);
    let text = r"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: octo/tool@v4
        with:
          fetch-depth: 0
      - run: echo hi
";
    let graph = lower(text, &source).graph.expect("lowers");
    assert_eq!(chain(&graph), vec![
        "build/step-1",
        "build/step-2",
        "build/step-1/post"
    ]);

    let main = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-1")
        .unwrap();
    assert_eq!(main.step.kind.to_string(), ACTION_KIND);
    let config = &main.step.config;
    assert_eq!(config["entry"], "dist/index.js");
    assert_eq!(config["action"]["sha"], TEST_SHA);
    assert_eq!(config["action"]["reference"]["owner"], "octo");
    assert_eq!(config["action"]["reference"]["ref"], "v4");
    // The caller's value wins over the default; a number is passed as text.
    assert_eq!(config["inputs"]["fetch-depth"], "0");
    // `github.token` is a secret reference, never a value.
    assert_eq!(
        config["inputs"]["token"],
        json!({ "$secret": "GITHUB_TOKEN" })
    );
    // `github.repository` is an expression over the run parameters.
    assert!(config["inputs"]["repository"].get("$expr").is_some());
    assert!(config.get("state").is_none(), "main has no earlier phase");

    let post = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-1/post")
        .unwrap();
    assert_eq!(post.step.kind.to_string(), ACTION_KIND);
    assert_eq!(post.step.config["entry"], "dist/cleanup.js");
    assert!(
        post.step.config["state"].get("$expr").is_some(),
        "post reads main's saved state"
    );
    assert!(post.run_on_cancel, "a default `post-if` is `always()`");

    let run = graph
        .nodes
        .iter()
        .find(|n| n.name == "build/step-2")
        .unwrap();
    assert_eq!(run.step.kind.to_string(), RUN_KIND);
    assert!(run.step.config.get("output_env_aliases").is_none());
    assert!(run.step.config["event"].get("$expr").is_some());
    let _ = STATE_OUTPUT_KEY;
}

#[test]
fn pre_nodes_come_first_and_post_nodes_last_in_reverse() {
    let source = MapActionSource::new()
        .with("acme/prepost@v1", TEST_SHA, WITH_PRE)
        .with("octo/tool@v4", TEST_SHA, CHECKOUT);
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo first
      - id: a
        uses: acme/prepost@v1
      - id: b
        uses: octo/tool@v4
";
    let graph = lower(text, &source).graph.expect("lowers");
    assert_eq!(chain(&graph), vec![
        "j/a/pre", "j/step-1", "j/a", "j/b", "j/b/post", "j/a/post"
    ]);
    let a = graph.nodes.iter().find(|n| n.name == "j/a").unwrap();
    assert!(
        a.step.config["state"].get("$expr").is_some(),
        "main reads pre's saved state"
    );
    let a_post = graph.nodes.iter().find(|n| n.name == "j/a/post").unwrap();
    assert!(
        a_post.run_on_cancel && a_post.precondition.is_none(),
        "a post node fires after a cancel and its gate decides; `post-if: success()` then reads false"
    );
    assert!(
        a_post.step.config.get("gate").is_some(),
        "the post-if is a config gate, not a precondition"
    );
}

#[test]
fn a_required_input_without_a_value_warns_and_lowers() {
    let source = MapActionSource::new().with(
        "acme/needs@v1",
        TEST_SHA,
        "inputs:\n  who:\n    required: true\nruns:\n  using: node20\n  main: index.js\n",
    );
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/needs@v1
";
    let lowered = lower(text, &source);
    // GitHub's runner warns about a missing required action input and runs
    // anyway; real workflows rely on that. (A workflow_call input stays an
    // error — GitHub fails those.)
    assert!(lowered.graph.is_some());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "gha.missing_input"),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn a_required_input_with_an_empty_default_is_satisfied() {
    let source = MapActionSource::new().with(
        "acme/needy@v1",
        TEST_SHA,
        "inputs:\n  github_token:\n    description: 'a token'\n    required: true\n    default: ''\nruns:\n  using: node20\n  main: index.js\n",
    );
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/needy@v1
";
    let lowered = lower(text, &source);
    let graph = lowered.graph.expect("an empty default satisfies the input");
    let node = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    assert_eq!(node.step.config["inputs"]["github_token"], "");
}

#[test]
fn a_required_input_with_a_null_default_is_still_missing() {
    let source = MapActionSource::new().with(
        "acme/nully@v1",
        TEST_SHA,
        "inputs:\n  who:\n    required: true\n    default:\nruns:\n  using: node20\n  main: index.js\n",
    );
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: acme/nully@v1
";
    let lowered = lower(text, &source);
    // A null default does not satisfy the input; the runner-faithful warning
    // still names it.
    assert!(lowered.graph.is_some());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.code == "gha.missing_input"),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn without_a_source_remote_actions_stay_unsupported() {
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: octo/tool@v4
";
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.unsupported_feature() == Some("action.remote")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

/// The two ways a source declines to serve a reference get their own codes: an
/// uncovered reference is `action.remote` and points at a refresh; a recorded
/// upstream failure (a private or removed repository) is `action.upstream_gone`
/// — the workflow is broken on GitHub itself — and carries the recorded error.
#[test]
fn an_unavailable_reference_is_unsupported_and_the_hint_says_why() {
    use frontend_gha::action::{ActionRef, ActionSourceError, PinnedAction};

    /// A partial source — a snapshot, say — that serves nothing, with or
    /// without a recorded reason.
    struct Offline(Option<&'static str>);
    impl Offline {
        fn decline(&self, reference: String) -> ActionSourceError {
            ActionSourceError::Unavailable {
                reference,
                reason: self.0.map(str::to_string),
            }
        }
    }
    impl frontend_gha::ActionSource for Offline {
        fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
            Err(self.decline(reference.to_string()))
        }
        fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
            Err(self.decline(pinned.reference().to_string()))
        }
    }

    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: octo/tool@v4
";
    for (reason, code, wants) in [
        (None, "action.remote", "refreshing it"),
        (
            Some("remote: Repository not found."),
            "action.upstream_gone",
            "refreshing the source will not help",
        ),
    ] {
        let lowered = load_with(
            ".github/workflows/ci.yml",
            text,
            &NoFiles,
            Some(&Offline(reason)),
        );
        assert!(lowered.graph.is_none());
        let diags = lowered.diagnostics.into_vec();
        let d = diags
            .iter()
            .find(|d| d.unsupported_feature() == Some(code))
            .unwrap_or_else(|| panic!("{diags:?}"));
        let hint = d.hint.as_deref().unwrap_or_default();
        assert!(hint.contains(wants), "{hint}");
        if let Some(reason) = reason {
            assert!(hint.contains(reason), "{hint}");
        }
    }
}

#[test]
fn an_unresolvable_reference_is_an_error_not_an_unsupported_feature() {
    let source = MapActionSource::new();
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - uses: nobody/nothing@v9
";
    let lowered = lower(text, &source);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .errors()
            .any(|d| d.to_string().contains("nobody/nothing@v9")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}

#[test]
fn secrets_lower_in_step_config_and_nowhere_else() {
    // Whole value: a `$secret` reference the process step resolves.
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
        env:
          T: ${{ github.token }}
";
    let graph = load(".github/workflows/ci.yml", text, &NoFiles)
        .graph
        .expect("lowers");
    let run = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    assert_eq!(
        run.step.config["env"]["T"],
        json!({ "$secret": "GITHUB_TOKEN" })
    );

    // Inside a larger string or expression in step config: lowers, as an
    // expression the step resolves the sentinel out of at spawn.
    let text = r#"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo "token is ${{ github.token }}"
        env:
          AUTH: ${{ github.server_url == 'https://github.com' && github.token || '' }}
"#;
    let graph = load(".github/workflows/ci.yml", text, &NoFiles)
        .graph
        .expect("lowers");
    let run = graph.nodes.iter().find(|n| n.name == "j/step-1").unwrap();
    assert!(run.step.config["run"].get("$expr").is_some());
    assert!(run.step.config["env"]["AUTH"].get("$expr").is_some());

    // Where the engine would evaluate it — an `if:` — a secret is rejected.
    let text = r"
on: push
jobs:
  j:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
        if: secrets.DEPLOY_KEY != ''
";
    let lowered = load(".github/workflows/ci.yml", text, &NoFiles);
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.unsupported_feature() == Some("secrets.expression")),
        "{:?}",
        lowered.diagnostics.into_vec()
    );
}
