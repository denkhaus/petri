//! The event loader: positioned YAML with anchors resolved.
//!
//! `marked_yaml`'s own loader rejects anchors and aliases, which real workflow
//! files use (`paths: &paths` … `paths: *paths`). This loader drives the same
//! event parser (`yaml_rust2`) and builds the same `marked_yaml` node types, but
//! keeps every anchored node and splices a copy in at each alias. The copy keeps
//! the anchor site's span: a diagnostic inside aliased content points at the one
//! place the content is written.
//!
//! The shape rules mirror `marked_yaml`'s loader (MIT) as `Document::parse` has
//! always applied them: the top level is a mapping, mapping keys are scalars,
//! tags are rejected, duplicate keys keep the last value, and only plain
//! (unquoted) scalars may type-coerce.

use std::collections::HashMap;

use marked_yaml::Node as MarkedNode;
use marked_yaml::types::{Marker, MarkedMappingNode, MarkedScalarNode, MarkedSequenceNode, Span};
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker as YamlMarker, ScanError, TScalarStyle};

/// Why a document did not load: a scanner error from the parser, or a shape the
/// loader rejects, at the position it sits.
pub enum ParseFailure {
    Scan(ScanError),
    Shape { line: u32, column: u32, message: String },
}

/// Parse one YAML document into a positioned root node, resolving aliases.
pub fn load(text: &str) -> Result<MarkedNode, ParseFailure> {
    let mut loader = Loader::default();
    let mut parser = Parser::new_from_str(text);
    parser.load(&mut loader, false).map_err(ParseFailure::Scan)?;
    if let Some((span, message)) = loader.error {
        return Err(ParseFailure::Shape {
            line: span.start().map_or(0, |m| m.line() as u32),
            column: span.start().map_or(0, |m| m.column() as u32),
            message,
        });
    }
    Ok(loader
        .root
        .unwrap_or_else(|| MarkedNode::from(MarkedMappingNode::new_empty(Span::new_blank()))))
}

/// A container being filled: its node accumulates entries in place, and the
/// anchor id (0 for none) names it once complete.
enum Frame {
    Mapping {
        node: MarkedMappingNode,
        /// A key waiting for its value.
        key: Option<MarkedScalarNode>,
        aid: usize,
    },
    Sequence {
        node: MarkedSequenceNode,
        aid: usize,
    },
}

#[derive(Default)]
struct Loader {
    frames: Vec<Frame>,
    /// Every anchored node, by the parser's anchor id, once complete.
    anchors: HashMap<usize, MarkedNode>,
    root: Option<MarkedNode>,
    error: Option<(Span, String)>,
}

impl Loader {
    fn fail(&mut self, mark: Marker, message: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some((Span::new_start(mark), message.into()));
        }
    }

    /// Whether the next value event would land in key position of a mapping.
    fn expecting_key(&self) -> bool {
        matches!(self.frames.last(), Some(Frame::Mapping { key: None, .. }))
    }

    /// Route a completed node to its container, or make it the root.
    fn feed(&mut self, node: MarkedNode, mark: Marker) {
        let rejected = match self.frames.last_mut() {
            Some(Frame::Mapping { node: map, key, .. }) => match key.take() {
                Some(key) => {
                    // Duplicate keys keep the last value, as the reader always has.
                    map.insert(key, node);
                    None
                }
                None => match node {
                    MarkedNode::Scalar(scalar) => {
                        *key = Some(scalar);
                        None
                    }
                    _ => Some("mapping keys must be scalars"),
                },
            },
            Some(Frame::Sequence { node: seq, .. }) => {
                seq.push(node);
                None
            }
            None => {
                if matches!(node, MarkedNode::Mapping(_)) {
                    self.root = Some(node);
                    None
                } else {
                    Some("the top level must be a mapping")
                }
            }
        };
        if let Some(message) = rejected {
            self.fail(mark, message);
        }
    }
}

impl MarkedEventReceiver for Loader {
    fn on_event(&mut self, event: Event, mark: YamlMarker) {
        if self.error.is_some() || self.root.is_some() {
            return;
        }
        let mark = Marker::new(0, mark.index(), mark.line(), mark.col() + 1);
        match event {
            Event::Nothing
            | Event::StreamStart
            | Event::DocumentStart
            | Event::DocumentEnd => {}
            Event::StreamEnd => {
                // An empty document reads as an empty mapping, as it always has.
                if self.frames.is_empty() {
                    self.root = Some(MarkedNode::from(MarkedMappingNode::new_empty(
                        Span::new_start(mark),
                    )));
                }
            }
            Event::MappingStart(aid, tag) => {
                if tag.is_some() {
                    self.fail(mark, "YAML tags are not supported");
                } else if self.expecting_key() {
                    self.fail(mark, "mapping keys must be scalars");
                } else {
                    self.frames.push(Frame::Mapping {
                        node: MarkedMappingNode::new_empty(Span::new_start(mark)),
                        key: None,
                        aid,
                    });
                }
            }
            Event::SequenceStart(aid, tag) => {
                if tag.is_some() {
                    self.fail(mark, "YAML tags are not supported");
                } else if self.expecting_key() {
                    self.fail(mark, "mapping keys must be scalars");
                } else if self.frames.is_empty() {
                    self.fail(mark, "the top level must be a mapping");
                } else {
                    self.frames.push(Frame::Sequence {
                        node: MarkedSequenceNode::new_empty(Span::new_start(mark)),
                        aid,
                    });
                }
            }
            Event::MappingEnd | Event::SequenceEnd => {
                let (node, aid) = match self.frames.pop() {
                    Some(Frame::Mapping { mut node, aid, .. }) => {
                        node.span_mut().set_end(Some(mark));
                        (MarkedNode::from(node), aid)
                    }
                    Some(Frame::Sequence { mut node, aid }) => {
                        node.span_mut().set_end(Some(mark));
                        (MarkedNode::from(node), aid)
                    }
                    None => return,
                };
                if aid != 0 {
                    self.anchors.insert(aid, node.clone());
                }
                self.feed(node, mark);
            }
            Event::Scalar(value, style, aid, tag) => {
                if tag.is_some() {
                    self.fail(mark, "YAML tags are not supported");
                    return;
                }
                let mut node = MarkedScalarNode::new(Span::new_start(mark), value);
                // Coercion prevention: only a plain (unquoted) scalar type-infers.
                node.set_coerce(matches!(style, TScalarStyle::Plain));
                let node = MarkedNode::from(node);
                if aid != 0 {
                    self.anchors.insert(aid, node.clone());
                }
                self.feed(node, mark);
            }
            Event::Alias(aid) => match self.anchors.get(&aid) {
                // The copy keeps the anchor site's span: a diagnostic inside
                // aliased content points at where the content is written.
                Some(node) => self.feed(node.clone(), mark),
                // The parser resolves alias names to ids of seen anchors, so
                // this arm should be unreachable; fail rather than guess.
                None => self.fail(mark, "unknown alias"),
            },
        }
    }
}
