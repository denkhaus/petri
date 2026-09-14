//! The DOT subset: lexer, parser, and the AST they produce.
//!
//! Written here rather than taken from `fabro-graphviz`, which depends on
//! Fabro's type crates and a vendored C Graphviz: a component names core and
//! nothing else. The subset is exactly what Fabro accepts.

pub mod ast;
mod lexer;
mod parser;

pub use ast::{
    AstValue, Attr, AttrBlock, DotGraph, EdgeStmt, Ident, NodeStmt, Statement, SubgraphStmt,
};
pub use parser::parse;
