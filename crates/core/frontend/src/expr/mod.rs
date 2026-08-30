//! The `${{ }}` expression grammar: one parser, semantics decided by the
//! lowering.
//!
//! The syntax is the one users of YAML CI systems already know. What an
//! operator *means* is the lowering's decision, and lowerings live with the
//! format that owns them: [`lower::strict`] here, for the native format, maps
//! `==` onto the engine's own `Eq`; the GitHub Actions frontend carries a
//! lowering onto the loose builtins in its own crate. No lowering evaluates
//! anything. Evaluation happens once, in the engine.

pub mod ast;
pub mod lexer;
pub mod lower;
pub mod parser;
pub mod print;

pub use ast::{BinaryOp, Expr, Literal, UnaryOp};
pub use lexer::{Token, TokenKind, lex};
pub use parser::{ParseError, parse};
pub use print::print;

/// Where an expression came from within a larger string, for spans and
/// templates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Literal text outside any `${{ }}`.
    Text(String),
    /// The source inside one `${{ }}`, braces stripped, with its byte offset in
    /// the original string.
    Expr { source: String, offset: usize },
}

/// Split a scalar into literal text and `${{ }}` expression segments.
///
/// Returns `Err(offset)` for an unterminated `${{`.
pub fn split_template(text: &str) -> Result<Vec<Segment>, usize> {
    let mut segments = Vec::new();
    let mut rest = text;
    let mut consumed = 0;
    while let Some(start) = rest.find("${{") {
        if start > 0 {
            segments.push(Segment::Text(rest[..start].to_string()));
        }
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else {
            return Err(consumed + start);
        };
        segments.push(Segment::Expr {
            source: after[..end].to_string(),
            offset: consumed + start + 3,
        });
        let advance = start + 3 + end + 2;
        rest = &rest[advance..];
        consumed += advance;
    }
    if !rest.is_empty() {
        segments.push(Segment::Text(rest.to_string()));
    }
    Ok(segments)
}

/// Whether a scalar contains any `${{`.
pub fn has_template(text: &str) -> bool {
    text.contains("${{")
}
