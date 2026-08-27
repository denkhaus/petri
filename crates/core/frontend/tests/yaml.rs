//! The positioned YAML reader: anchors resolve, positions and coercion rules
//! hold, and malformed documents stay loud.

use frontend::diag::Diagnostics;
use frontend::yaml::Document;

fn parse(text: &str) -> Document {
    let mut diags = Diagnostics::new();
    let doc = Document::parse("test.yml", text, &mut diags);
    for d in diags.iter() {
        eprintln!("{d}");
    }
    doc.expect("expected the document to parse")
}

fn parse_error(text: &str) -> Vec<frontend::Diagnostic> {
    let mut diags = Diagnostics::new();
    let doc = Document::parse("test.yml", text, &mut diags);
    assert!(doc.is_none(), "expected a parse failure");
    diags.into_vec()
}

/// The corpus shape that motivated anchor support: a block sequence anchored at
/// one trigger and aliased at another.
#[test]
fn anchors_and_aliases_resolve_to_copies() {
    let doc = parse(
        r#"
on:
  pull_request:
    paths: &paths
      - "Python/**"
      - "Tools/jit/**"
  push:
    paths: *paths
jobs: {}
"#,
    );
    let root = doc.root().as_mapping().unwrap();
    let on = root.get("on").unwrap().as_mapping().unwrap();
    let read = |trigger: &str| {
        let paths = on
            .get(trigger)
            .unwrap()
            .as_mapping()
            .unwrap()
            .get("paths")
            .unwrap();
        let items: Vec<String> = paths
            .as_sequence()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_string())
            .collect();
        (items, paths.span())
    };
    let (anchored, anchored_span) = read("pull_request");
    let (aliased, aliased_span) = read("push");
    assert_eq!(anchored, ["Python/**", "Tools/jit/**"]);
    assert_eq!(aliased, anchored);
    // The copy keeps the anchor site's span: a diagnostic inside aliased content
    // points at the one place the content is written.
    assert_eq!(aliased_span.line, anchored_span.line);
}

#[test]
fn anchored_mappings_and_scalars_resolve_too() {
    let doc = parse(
        "defaults: &d\n  shell: bash\n  working-directory: sub\nagain: *d\nname: &n ci\nalias: *n\n",
    );
    let root = doc.root().as_mapping().unwrap();
    let again = root.get("again").unwrap().as_mapping().unwrap();
    assert_eq!(again.get("shell").unwrap().as_str(), Some("bash"));
    assert_eq!(again.get("working-directory").unwrap().as_str(), Some("sub"));
    assert_eq!(root.get("alias").unwrap().as_str(), Some("ci"));
}

/// The rules the reader has always applied hold in the anchor-resolving loader:
/// quoted scalars never type-infer, duplicate keys keep the last value, and
/// nodes know their positions.
#[test]
fn coercion_positions_and_duplicates_are_unchanged() {
    let doc = parse("a: true\nb: 'true'\nc: 1\nc: 2\n");
    let root = doc.root().as_mapping().unwrap();
    let scalar = |key: &str| root.get(key).unwrap().as_scalar().unwrap().to_json();
    assert_eq!(scalar("a"), serde_json::json!(true));
    assert_eq!(scalar("b"), serde_json::json!("true"));
    assert_eq!(scalar("c"), serde_json::json!(2));

    let b = root.get("b").unwrap().span();
    assert_eq!((b.line, b.column), (2, 4));
}

#[test]
fn malformed_documents_stay_loud() {
    for (text, wants) in [
        ("just a scalar", "top level"),
        ("- a\n- b\n", "top level"),
        ("? {a: 1}\n: value\n", "keys must be scalars"),
        ("a: !!str tagged\n", "tag"),
        ("a: *ghost\n", "anchor"),
        ("a: [\n", "syntax"),
    ] {
        let diags = parse_error(text);
        assert!(
            diags.iter().any(|d| d.code == "yaml.syntax"),
            "{text}: {diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.to_lowercase().contains(wants) || wants == "syntax"),
            "{text}: {diags:?}"
        );
    }
}
