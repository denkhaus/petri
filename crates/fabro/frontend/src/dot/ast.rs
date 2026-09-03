//! The DOT subset, as parsed. Positions ride every node, edge and attribute
//! so a diagnostic can point at the line that caused it.

use frontend::Span;

/// A parsed attribute value before semantic interpretation.
#[derive(Clone, Debug, PartialEq)]
pub enum AstValue {
    /// A double-quoted string, escapes resolved.
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// A bare word used as a value: a shape name, a direction, `900s`.
    Ident(String),
}

impl AstValue {
    /// The value as text, however it was written.
    pub fn as_text(&self) -> String {
        match self {
            Self::Str(s) | Self::Ident(s) => s.clone(),
            Self::Int(n) => n.to_string(),
            Self::Float(f) => f.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }
}

/// One `key = value` pair, with the key's position.
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub key:   String,
    pub value: AstValue,
    pub span:  Span,
}

pub type AttrBlock = Vec<Attr>;

/// A node id with the position it was written at.
#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// `id [attrs]?`
#[derive(Clone, Debug, PartialEq)]
pub struct NodeStmt {
    pub id:    Ident,
    pub attrs: Option<AttrBlock>,
}

/// `a -> b -> c [attrs]?`
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeStmt {
    /// At least two.
    pub nodes: Vec<Ident>,
    pub attrs: Option<AttrBlock>,
}

/// `subgraph name? { statements }`
#[derive(Clone, Debug, PartialEq)]
pub struct SubgraphStmt {
    pub name:       Option<Ident>,
    pub statements: Vec<Statement>,
    pub span:       Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// `graph [attrs]`
    GraphAttrs(AttrBlock),
    /// `node [attrs]`
    NodeDefaults(AttrBlock),
    /// `edge [attrs]`
    EdgeDefaults(AttrBlock),
    Subgraph(SubgraphStmt),
    Node(NodeStmt),
    Edge(EdgeStmt),
    /// A bare `key = value` at graph level.
    GraphAttr(Attr),
}

/// `digraph name { statements }`
#[derive(Clone, Debug, PartialEq)]
pub struct DotGraph {
    pub name:       Ident,
    pub statements: Vec<Statement>,
}
