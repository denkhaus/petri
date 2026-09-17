//! The admission seam: `Runtime::check` runs every registered
//! `AdmissionPass` over the root graph and the pre-lowered children after the
//! step registry accepted them, hands each pass the runtime's static
//! capabilities, turns a problem into a diagnostic on the node's span, and
//! follows a changed child's new digest through the parent's reference.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, fs, process};

use runtime::frontend::{CompileInputs, Diagnostics, FileSource, Frontend, Lowered, graph_digest};
use runtime::ir::{Graph, GraphBuilder, ScopeId, StepRef};
use runtime::steps::Capabilities;
use runtime::{AdmissionPass, AdmissionProblem, Runtime};
use serde_json::{Value, json};

/// The key the root's node holds its child's digest under.
const CHILD_KEY: &str = "child";

/// A host service a pass reads.
struct Catalog(&'static str);

/// A frontend that lowers a fixed root — one `noop` node `a` at line 3,
/// column 5, naming a child graph by digest — and the child beside it.
/// `kind` is the step kind the root's node gets, so a test can make the
/// registry refuse the graph.
struct Fixed {
    kind: &'static str,
}

impl Fixed {
    fn child() -> Graph {
        let mut b = GraphBuilder::new();
        let inner = b.add_node(
            "inner",
            ScopeId::new(0),
            StepRef::new("noop", json!({ "model": "alias" })),
        );
        b.mark_entry(inner);
        b.build()
    }

    fn root(&self, child: &Graph) -> Graph {
        let mut b = GraphBuilder::new();
        let a = b.add_node(
            "a",
            ScopeId::new(0),
            StepRef::new(
                self.kind,
                json!({ "model": "alias", CHILD_KEY: graph_digest(child) }),
            ),
        );
        b.set_meta(a, json!({ "span": { "line": 3, "column": 5 } }));
        b.mark_entry(a);
        b.build()
    }
}

impl Frontend for Fixed {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `Frontend` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "fixed"
    }

    fn claims(&self, path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "fixed")
    }

    fn load(&self, _: &str, _: &str, _: &dyn FileSource, _: &CompileInputs) -> Lowered {
        let child = Self::child();
        let root = self.root(&child);
        Lowered::with_children(root, vec![child], Diagnostics::new())
    }
}

/// A pass that pins every `model` to the catalog it was given, counting its
/// calls, or refuses the node `refuse` names.
struct Pin {
    calls:  Arc<AtomicUsize>,
    refuse: Option<(Option<&'static str>, &'static str)>,
}

impl AdmissionPass for Pin {
    fn admit(&self, graph: &mut Graph, caps: &Capabilities) -> Vec<AdmissionProblem> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((node, code)) = self.refuse {
            return vec![AdmissionProblem::new(code, node, "refused by the pass")];
        }
        let Some(catalog) = caps.get::<Catalog>() else {
            return Vec::new();
        };
        for node in &mut graph.body.nodes {
            if let Some(config) = node.step.config.as_object_mut() {
                config.insert("model".into(), Value::String(catalog.0.to_owned()));
            }
        }
        Vec::new()
    }
}

fn workflow_file(label: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("petri-admission-{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the test dir");
    let file = dir.join("wf.fixed");
    fs::write(&file, "fixed").expect("the workflow file");
    file
}

fn check(rt: &Runtime, file: &Path) -> Lowered {
    rt.check(file, None, None, &CompileInputs::new())
        .expect("loads")
}

fn config_of<'a>(graph: &'a Graph, name: &str) -> &'a Value {
    &graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == name)
        .expect("the node")
        .step
        .config
}

/// A pass sees the static capabilities and changes the root and the child;
/// the child's digest moves and the root's reference follows it.
#[tokio::test]
async fn a_pass_rewrites_the_root_and_the_child_and_the_reference_follows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rt = Runtime::standard()
        .frontend(Fixed { kind: "noop" })
        .capability(Catalog("provider/concrete"))
        .admission(Pin {
            calls:  calls.clone(),
            refuse: None,
        });
    let lowered = check(&rt, &workflow_file("rewrite"));
    assert!(lowered.diagnostics.is_empty(), "{:?}", lowered.diagnostics);
    let graph = lowered.graph.expect("admitted");
    assert_eq!(lowered.children.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the root and the child");
    let root = config_of(&graph, "a");
    assert_eq!(root["model"], json!("provider/concrete"));
    let child = &lowered.children[0];
    assert_eq!(
        config_of(child, "inner")["model"],
        json!("provider/concrete")
    );
    let before = graph_digest(&Fixed::child());
    let after = graph_digest(child);
    assert_ne!(before, after, "the child's content changed");
    assert_eq!(root[CHILD_KEY], json!(after), "the reference follows");
}

/// A problem on a node is an error diagnostic at the node's `meta.span`;
/// one on the graph is at the file. Neither hands out a graph.
#[tokio::test]
async fn a_problem_is_a_diagnostic_on_the_nodes_span_and_withholds_the_graph() {
    let file = workflow_file("refuse");
    let rt = Runtime::standard()
        .frontend(Fixed { kind: "noop" })
        .admission(Pin {
            calls:  Arc::new(AtomicUsize::new(0)),
            refuse: Some((Some("a"), "test.refused")),
        });
    let lowered = check(&rt, &file);
    assert!(lowered.graph.is_none());
    assert!(lowered.children.is_empty());
    let errors: Vec<_> = lowered.diagnostics.errors().collect();
    // The root's `a`, and the child (which has no `a`) at the file.
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert_eq!(errors[0].code, "test.refused");
    assert_eq!(errors[0].message, "refused by the pass");
    assert_eq!(errors[0].span.file.as_str(), file.to_string_lossy());
    assert_eq!((errors[0].span.line, errors[0].span.column), (3, 5));
    assert_eq!((errors[1].span.line, errors[1].span.column), (0, 0));

    let rt = Runtime::standard()
        .frontend(Fixed { kind: "noop" })
        .admission(Pin {
            calls:  Arc::new(AtomicUsize::new(0)),
            refuse: Some((None, "test.graph")),
        });
    let lowered = check(&rt, &file);
    let errors: Vec<_> = lowered.diagnostics.errors().collect();
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .all(|e| e.code == "test.graph" && e.span.line == 0)
    );
}

/// A graph the registry refuses never reaches a pass.
#[tokio::test]
async fn a_registry_error_stops_before_the_passes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rt = Runtime::standard()
        .frontend(Fixed { kind: "nonesuch" })
        .admission(Pin {
            calls:  calls.clone(),
            refuse: None,
        });
    let lowered = check(&rt, &workflow_file("registry"));
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .errors()
            .any(|e| e.code == "step.unknown_kind"),
        "{:?}",
        lowered.diagnostics
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Without a pass the graph is what the frontend lowered.
#[tokio::test]
async fn without_a_pass_the_graph_is_the_frontends() {
    let rt = Runtime::standard().frontend(Fixed { kind: "noop" });
    let lowered = check(&rt, &workflow_file("plain"));
    let graph = lowered.graph.expect("admitted");
    assert_eq!(config_of(&graph, "a")["model"], json!("alias"));
    assert_eq!(
        config_of(&graph, "a")[CHILD_KEY],
        json!(graph_digest(&lowered.children[0]))
    );
}
