//! Local composite actions: `uses: ./path/to/action`.

use frontend::diag::{Diagnostics, Span};
use frontend::yaml::{Document, Node};

use crate::FileSource;
use crate::model::Step;

/// How deep composites may nest before the lowering reports rather than recurses.
pub const MAX_DEPTH: usize = 10;

pub struct Action<'a> {
    pub inputs: Vec<Input<'a>>,
    pub outputs: Vec<(String, Node<'a>)>,
    pub steps: Vec<Step<'a>>,
    pub span: Span,
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

/// Read an action document as a composite. Anything else is rejected with where it
/// is headed.
pub fn read<'a>(
    doc: &'a Document,
    uses_span: &Span,
    diags: &mut Diagnostics,
) -> Option<Action<'a>> {
    let root = doc.root();
    let m = root.expect_mapping(diags, "an action")?;
    let Some(runs) = m.get("runs").and_then(|r| r.as_mapping()) else {
        diags.error("gha.bad_action", root.span(), "an action needs `runs:`");
        return None;
    };
    let using = runs.get("using").and_then(|u| u.as_str()).unwrap_or("");
    match using {
        "composite" => {}
        u if u.starts_with("node") => {
            diags.unsupported(
                "action.javascript",
                uses_span.clone(),
                format!("a JavaScript action (`runs.using: {u}`)"),
                "the JS action host is package 04",
            );
            return None;
        }
        "docker" => {
            diags.unsupported(
                "action.docker",
                uses_span.clone(),
                "a Docker container action",
                "Docker actions are v2",
            );
            return None;
        }
        other => {
            diags.error(
                "gha.bad_action",
                root.span(),
                format!("unknown `runs.using: {other}`"),
            );
            return None;
        }
    }

    let mut inputs = Vec::new();
    if let Some(inputs_node) = m.get("inputs")
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
