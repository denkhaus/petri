//! Action manifests: `action.yml`, read for a `uses:` step.
//!
//! A composite action is inlined into the job, so its steps are read here in full. A
//! Node action becomes a `github/action` node, so only its entry points and inputs
//! matter. Docker actions are recognised and rejected.

use frontend::FileSource;
use frontend::diag::{Diagnostics, Span};
use frontend::yaml::{Document, Node};

use crate::model::Step;

/// How deep composites may nest before the lowering reports rather than recurses.
pub const MAX_DEPTH: usize = 10;

/// What `action.yml` declares.
pub struct Manifest<'a> {
    pub inputs: Vec<Input<'a>>,
    pub runs: Runs<'a>,
    pub span: Span,
}

pub enum Runs<'a> {
    Composite(Action<'a>),
    Node(NodeAction),
    Docker,
}

/// A composite action's steps and outputs.
pub struct Action<'a> {
    pub inputs: Vec<Input<'a>>,
    pub outputs: Vec<(String, Node<'a>)>,
    pub steps: Vec<Step<'a>>,
    pub span: Span,
}

/// A JavaScript action's entry points. Owned, so it outlives the document it came
/// from: the pre and post nodes are placed away from the main one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAction {
    /// `node20`, `node24`, …
    pub runtime: String,
    pub main: String,
    pub pre: Option<String>,
    pub pre_if: Option<String>,
    pub post: Option<String>,
    pub post_if: Option<String>,
}

pub struct Input<'a> {
    pub name: String,
    pub default: Option<Node<'a>>,
    pub required: bool,
}

/// Classify a `uses:` reference.
pub enum Uses {
    /// `./path` — a composite in this repository.
    Local(String),
    /// `docker://image` or a local action whose `runs.using` is `docker`.
    Docker(String),
    /// `owner/repo[/path]@ref`.
    Remote(String),
}

pub fn classify(reference: &str) -> Uses {
    if let Some(rest) = reference.strip_prefix("docker://") {
        Uses::Docker(rest.to_string())
    } else if reference.starts_with("./") || reference.starts_with('.') {
        Uses::Local(
            reference
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_string(),
        )
    } else {
        Uses::Remote(reference.to_string())
    }
}

/// Read `action.yml` (or `.yaml`) under `path`. Returns the parsed document so the
/// caller can hold it while lowering.
pub fn read_document(
    files: &dyn FileSource,
    path: &str,
    span: &Span,
    diags: &mut Diagnostics,
) -> Option<Document> {
    // `uses: ./` is the repository root itself.
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let candidates = [
        format!("{prefix}action.yml"),
        format!("{prefix}action.yaml"),
    ];
    for candidate in &candidates {
        if let Some(text) = files.read(candidate) {
            return Document::parse(candidate, &text, diags);
        }
    }
    // GitHub resolves `./path` against the *workspace* after checkout, so a local
    // action can live in a directory that only exists at run time (`./localClone`,
    // `./node/.github/actions/x`). Lowering is static and cannot see it.
    diags.unsupported(
        "action.local_missing",
        span.clone(),
        format!("no `action.yml` or `action.yaml` under `{path}` in the repository"),
        "GitHub resolves `./` actions against the checked-out workspace at run time; lowering is static, so \
         the action must exist in the repository at that path (a spec finding: run-time action resolution)",
    );
    None
}

/// Read an action document: its inputs and what `runs:` says it is.
pub fn read_manifest<'a>(doc: &'a Document, diags: &mut Diagnostics) -> Option<Manifest<'a>> {
    let root = doc.root();
    let m = root.expect_mapping(diags, "an action")?;
    let Some(runs) = m.get("runs").and_then(|r| r.as_mapping()) else {
        diags.error("gha.bad_action", root.span(), "an action needs `runs:`");
        return None;
    };
    let inputs = read_inputs(m.get("inputs"), diags);
    let using = runs.get("using").and_then(|u| u.as_str()).unwrap_or("");
    let text = |key: &str| runs.get(key).and_then(|n| n.as_str()).map(str::to_string);
    let runs = match using {
        "composite" => Runs::Composite(read_composite(doc, diags)?),
        u if u.starts_with("node") => {
            let Some(main) = text("main") else {
                diags.error(
                    "gha.bad_action",
                    root.span(),
                    format!("a `runs.using: {u}` action needs `runs.main`"),
                );
                return None;
            };
            Runs::Node(NodeAction {
                runtime: u.to_string(),
                main,
                pre: text("pre"),
                pre_if: text("pre-if"),
                post: text("post"),
                post_if: text("post-if"),
            })
        }
        "docker" => Runs::Docker,
        other => {
            diags.error(
                "gha.bad_action",
                root.span(),
                format!("unknown `runs.using: {other}`"),
            );
            return None;
        }
    };
    Some(Manifest {
        inputs,
        runs,
        span: root.span(),
    })
}

fn read_inputs<'a>(node: Option<Node<'a>>, diags: &mut Diagnostics) -> Vec<Input<'a>> {
    let mut inputs = Vec::new();
    if let Some(inputs_node) = node
        && let Some(im) = inputs_node.expect_mapping(diags, "action `inputs`")
    {
        for (name, spec) in im.iter() {
            let (default, required) = match spec.as_mapping() {
                Some(sm) => (
                    sm.get("default"),
                    sm.get("required")
                        .and_then(|r| r.as_scalar())
                        .and_then(|s| s.as_bool())
                        .unwrap_or(false),
                ),
                None => (None, false),
            };
            inputs.push(Input {
                name: name.to_string(),
                default,
                required,
            });
        }
    }
    inputs
}

/// Read a composite action's steps and outputs. `runs.using: composite` is known.
fn read_composite<'a>(doc: &'a Document, diags: &mut Diagnostics) -> Option<Action<'a>> {
    let root = doc.root();
    let m = root.expect_mapping(diags, "an action")?;
    let runs = m.get("runs").and_then(|r| r.as_mapping())?;
    let inputs = read_inputs(m.get("inputs"), diags);

    let mut outputs = Vec::new();
    if let Some(outputs_node) = m.get("outputs")
        && let Some(om) = outputs_node.expect_mapping(diags, "action `outputs`")
    {
        for (name, spec) in om.iter() {
            match spec.as_mapping().and_then(|sm| sm.get("value")) {
                Some(value) => outputs.push((name.to_string(), value)),
                None => diags.error(
                    "gha.bad_action",
                    spec.span(),
                    format!("composite output `{name}` needs a `value`"),
                ),
            }
        }
    }

    let mut steps = Vec::new();
    match runs.get("steps") {
        None => diags.error(
            "gha.bad_action",
            root.span(),
            "a composite action needs `runs.steps`",
        ),
        Some(s) => {
            if let Some(seq) = s.expect_sequence(diags, "`runs.steps`") {
                for (index, step) in seq.iter().enumerate() {
                    if let Some(step) = crate::model::read_step("<composite>", index, step, diags) {
                        if step.run.is_some() && step.shell.is_none() {
                            diags.error(
                                "gha.bad_action",
                                step.span.clone(),
                                "a composite `run:` step must set `shell:`",
                            );
                        }
                        steps.push(step);
                    }
                }
            }
        }
    }

    Some(Action {
        inputs,
        outputs,
        steps,
        span: root.span(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Document, Diagnostics) {
        let mut diags = Diagnostics::new();
        let doc = Document::parse("action.yml", text, &mut diags).expect("parses");
        (doc, diags)
    }

    #[test]
    fn reads_a_node_action() {
        let (doc, mut diags) = parse(
            r#"
name: Hello
inputs:
  who:
    default: World
  token:
    required: true
runs:
  using: node20
  main: dist/index.js
  post: dist/post.js
  post-if: success()
"#,
        );
        let manifest = read_manifest(&doc, &mut diags).expect("a manifest");
        assert!(diags.is_empty(), "{:?}", diags.into_vec());
        assert_eq!(manifest.inputs.len(), 2);
        assert!(manifest.inputs[1].required);
        let Runs::Node(node) = manifest.runs else {
            panic!("expected a node action");
        };
        assert_eq!(
            node,
            NodeAction {
                runtime: "node20".into(),
                main: "dist/index.js".into(),
                pre: None,
                pre_if: None,
                post: Some("dist/post.js".into()),
                post_if: Some("success()".into()),
            }
        );
    }

    #[test]
    fn a_node_action_needs_main() {
        let (doc, mut diags) = parse("runs:\n  using: node20\n");
        assert!(read_manifest(&doc, &mut diags).is_none());
        assert!(diags.has_errors());
    }
}
