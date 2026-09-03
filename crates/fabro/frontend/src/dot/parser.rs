//! A recursive-descent parser for the DOT subset Fabro accepts.
//!
//! The subset: one `digraph`, graph/node/edge attribute statements, bare
//! `key = value` graph attributes, node statements, chained edges, and
//! `subgraph` blocks. Anything else Graphviz allows — ports, HTML strings,
//! undirected graphs, `strict`, anonymous subgraphs, subgraphs as edge
//! endpoints — is refused with an `unsupported.dot.*` diagnostic.

use frontend::{Diagnostic, Span};

use super::ast::{
    AstValue, Attr, AttrBlock, DotGraph, EdgeStmt, Ident, NodeStmt, Statement, SubgraphStmt,
};
use super::lexer::{LexErrorKind, Tok, Token, lex};

/// Parse `text`. The one diagnostic a failure produces is the first problem
/// found: a parse error has no sensible recovery in a graph language.
pub fn parse(file: &str, text: &str) -> Result<DotGraph, Diagnostic> {
    let tokens = match lex(text) {
        Ok(tokens) => tokens,
        Err(error) => {
            let span = Span::new(file, error.line, error.column);
            return Err(match error.kind {
                LexErrorKind::HtmlString => Diagnostic::unsupported(
                    "dot.html_string",
                    span,
                    error.kind.to_string(),
                    "write the label as a double-quoted string",
                ),
                LexErrorKind::UndirectedEdge => Diagnostic::unsupported(
                    "dot.undirected",
                    span,
                    error.kind.to_string(),
                    "use `->`",
                ),
                _ => Diagnostic::error("dot.syntax", span, error.kind.to_string()),
            });
        }
    };
    let mut parser = Parser {
        file,
        tokens,
        pos: 0,
    };
    parser.graph()
}

struct Parser<'a> {
    file:   &'a str,
    tokens: Vec<Token>,
    pos:    usize,
}

type Parsed<T> = Result<T, Diagnostic>;

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn peek_at(&self, offset: usize) -> Option<&Tok> {
        self.tokens.get(self.pos + offset).map(|t| &t.tok)
    }

    fn span_here(&self) -> Span {
        self.tokens.get(self.pos).map_or_else(
            || {
                self.tokens.last().map_or_else(
                    || Span::file(self.file),
                    |t| Span::new(self.file, t.line, t.column),
                )
            },
            |t| Span::new(self.file, t.line, t.column),
        )
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn syntax(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error("dot.syntax", self.span_here(), message)
    }

    fn unsupported(&self, feature: &str, message: impl Into<String>, hint: &str) -> Diagnostic {
        Diagnostic::unsupported(feature, self.span_here(), message, hint)
    }

    fn expect(&mut self, want: &Tok, what: &str) -> Parsed<Token> {
        match self.peek() {
            Some(tok) if tok == want => Ok(self.advance().expect("peeked")),
            Some(tok) => Err(self.syntax(format!("expected {what}, found {tok}"))),
            None => Err(self.syntax(format!("expected {what}, found the end of the file"))),
        }
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Whether the next token is the keyword `word`, case-insensitively, as
    /// DOT keywords are.
    fn at_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
    }

    fn graph(&mut self) -> Parsed<DotGraph> {
        if self.at_keyword("strict") {
            return Err(self.unsupported(
                "dot.strict",
                "`strict` graphs are not supported",
                "drop `strict`; Fabro graphs never repeat an edge on purpose",
            ));
        }
        if self.at_keyword("graph") {
            return Err(self.unsupported(
                "dot.undirected",
                "an undirected `graph` is not a workflow",
                "write `digraph`",
            ));
        }
        if !self.at_keyword("digraph") {
            return Err(self.syntax("a Fabro workflow starts with `digraph`"));
        }
        self.advance();
        let name = match self.peek() {
            Some(Tok::LBrace) => Ident {
                name: String::new(),
                span: self.span_here(),
            },
            _ => self.ident("the graph name")?,
        };
        self.expect(&Tok::LBrace, "`{`")?;
        let statements = self.statements()?;
        self.expect(&Tok::RBrace, "`}`")?;
        if let Some(tok) = self.peek() {
            return Err(self.syntax(format!("unexpected {tok} after the graph")));
        }
        Ok(DotGraph { name, statements })
    }

    /// Statements up to, not including, the closing `}`.
    fn statements(&mut self) -> Parsed<Vec<Statement>> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.syntax("expected `}` before the end of the file")),
                Some(Tok::RBrace) => return Ok(out),
                Some(Tok::Semi) => {
                    self.advance();
                }
                Some(_) => out.push(self.statement()?),
            }
        }
    }

    fn statement(&mut self) -> Parsed<Statement> {
        if self.at_keyword("graph") {
            self.advance();
            let attrs = self.attr_block()?;
            self.eat(&Tok::Semi);
            return Ok(Statement::GraphAttrs(attrs));
        }
        if self.at_keyword("node") {
            self.advance();
            let attrs = self.attr_block()?;
            self.eat(&Tok::Semi);
            return Ok(Statement::NodeDefaults(attrs));
        }
        if self.at_keyword("edge") {
            self.advance();
            let attrs = self.attr_block()?;
            self.eat(&Tok::Semi);
            return Ok(Statement::EdgeDefaults(attrs));
        }
        if self.at_keyword("subgraph") {
            return self.subgraph();
        }
        match self.peek() {
            Some(Tok::LBrace) => Err(self.unsupported(
                "dot.anonymous_subgraph",
                "anonymous `{ ... }` subgraphs are not supported",
                "name the subgraph: `subgraph cluster_name { ... }`",
            )),
            Some(Tok::Word(_) | Tok::Quoted(_)) => {
                if matches!(self.peek_at(1), Some(Tok::Eq)) {
                    let attr = self.attr()?;
                    self.eat(&Tok::Semi);
                    return Ok(Statement::GraphAttr(attr));
                }
                self.node_or_edge()
            }
            Some(tok) => Err(self.syntax(format!("expected a statement, found {tok}"))),
            None => Err(self.syntax("expected a statement")),
        }
    }

    fn subgraph(&mut self) -> Parsed<Statement> {
        let span = self.span_here();
        self.advance();
        let name = match self.peek() {
            Some(Tok::Word(_) | Tok::Quoted(_)) => Some(self.ident("the subgraph name")?),
            _ => None,
        };
        self.expect(&Tok::LBrace, "`{`")?;
        let statements = self.statements()?;
        self.expect(&Tok::RBrace, "`}`")?;
        if matches!(self.peek(), Some(Tok::Arrow)) {
            return Err(self.unsupported(
                "dot.subgraph_edge",
                "a subgraph cannot be an edge endpoint",
                "write one edge per node",
            ));
        }
        self.eat(&Tok::Semi);
        Ok(Statement::Subgraph(SubgraphStmt {
            name,
            statements,
            span,
        }))
    }

    fn node_or_edge(&mut self) -> Parsed<Statement> {
        let first = self.ident("a node id")?;
        self.reject_port()?;
        if !matches!(self.peek(), Some(Tok::Arrow)) {
            let attrs = self.opt_attr_block()?;
            self.eat(&Tok::Semi);
            return Ok(Statement::Node(NodeStmt { id: first, attrs }));
        }
        let mut nodes = vec![first];
        while self.eat(&Tok::Arrow) {
            if matches!(self.peek(), Some(Tok::LBrace)) || self.at_keyword("subgraph") {
                return Err(self.unsupported(
                    "dot.subgraph_edge",
                    "a subgraph cannot be an edge endpoint",
                    "write one edge per node",
                ));
            }
            nodes.push(self.ident("a node id after `->`")?);
            self.reject_port()?;
        }
        let attrs = self.opt_attr_block()?;
        self.eat(&Tok::Semi);
        Ok(Statement::Edge(EdgeStmt { nodes, attrs }))
    }

    fn reject_port(&self) -> Parsed<()> {
        if matches!(self.peek(), Some(Tok::Colon)) {
            return Err(self.unsupported(
                "dot.port",
                "node ports (`node:port`) are not supported",
                "drop the port; Fabro routes on edges, not on node ports",
            ));
        }
        Ok(())
    }

    fn ident(&mut self, what: &str) -> Parsed<Ident> {
        let span = self.span_here();
        match self.advance().map(|t| t.tok) {
            Some(Tok::Word(name) | Tok::Quoted(name)) => Ok(Ident { name, span }),
            Some(tok) => Err(Diagnostic::error(
                "dot.syntax",
                span,
                format!("expected {what}, found {tok}"),
            )),
            None => Err(Diagnostic::error(
                "dot.syntax",
                span,
                format!("expected {what}, found the end of the file"),
            )),
        }
    }

    fn opt_attr_block(&mut self) -> Parsed<Option<AttrBlock>> {
        if matches!(self.peek(), Some(Tok::LBracket)) {
            return Ok(Some(self.attr_block()?));
        }
        Ok(None)
    }

    /// `[ attr ((,|;)? attr)* ]` — the separator is optional, as in DOT.
    fn attr_block(&mut self) -> Parsed<AttrBlock> {
        self.expect(&Tok::LBracket, "`[`")?;
        let mut attrs = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::RBracket) => {
                    self.advance();
                    return Ok(attrs);
                }
                Some(Tok::Comma | Tok::Semi) => {
                    self.advance();
                }
                Some(Tok::Word(_) | Tok::Quoted(_)) => attrs.push(self.attr()?),
                Some(tok) => {
                    return Err(self.syntax(format!("expected an attribute or `]`, found {tok}")));
                }
                None => return Err(self.syntax("expected `]` before the end of the file")),
            }
        }
    }

    fn attr(&mut self) -> Parsed<Attr> {
        let key = self.ident("an attribute name")?;
        self.expect(&Tok::Eq, "`=`")?;
        let value = match self.advance().map(|t| t.tok) {
            Some(Tok::Quoted(s)) => AstValue::Str(s),
            Some(Tok::Word(w)) => word_value(&w),
            Some(tok) => {
                return Err(
                    self.syntax(format!("expected a value for `{}`, found {tok}", key.name))
                );
            }
            None => return Err(self.syntax(format!("expected a value for `{}`", key.name))),
        };
        Ok(Attr {
            key: key.name,
            value,
            span: key.span,
        })
    }
}

/// A bare word as a value: booleans and numbers by shape, else an identifier.
fn word_value(word: &str) -> AstValue {
    match word {
        "true" => return AstValue::Bool(true),
        "false" => return AstValue::Bool(false),
        _ => {}
    }
    if let Ok(n) = word.parse::<i64>() {
        return AstValue::Int(n);
    }
    let numeric = word.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.');
    if numeric
        && let Ok(f) = word.parse::<f64>()
        && f.is_finite()
    {
        return AstValue::Float(f);
    }
    AstValue::Ident(word.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> DotGraph {
        parse("w.fabro", text).unwrap_or_else(|d| panic!("{d}"))
    }

    #[test]
    fn parses_the_hello_shape() {
        let g = parsed(
            r#"digraph Hello {
                graph [goal="Say hello"]
                rankdir=LR
                start [shape=Mdiamond, label="Start"]
                exit  [shape=Msquare, label="Exit"]
                greet [label="Greet", prompt="Add a haiku"]
                start -> greet -> exit
            }"#,
        );
        assert_eq!(g.name.name, "Hello");
        assert_eq!(g.statements.len(), 6);
        let Statement::Edge(edge) = &g.statements[5] else {
            panic!("an edge chain");
        };
        assert_eq!(edge.nodes.len(), 3);
        assert_eq!(
            (edge.nodes[2].span.line, edge.nodes[2].span.column),
            (7, 35)
        );
    }

    #[test]
    fn attribute_separators_are_optional() {
        let g = parsed("digraph G {\n n [\n a=1\n b=\"x\"; c=true,\n d=900s\n]\n}");
        let Statement::Node(node) = &g.statements[0] else {
            panic!("a node");
        };
        let attrs = node.attrs.as_ref().expect("attrs");
        assert_eq!(attrs.len(), 4);
        assert_eq!(attrs[0].value, AstValue::Int(1));
        assert_eq!(attrs[2].value, AstValue::Bool(true));
        assert_eq!(attrs[3].value, AstValue::Ident("900s".into()));
    }

    #[test]
    fn subgraphs_nest_and_keep_their_name() {
        let g = parsed("digraph G { subgraph cluster_a { label=\"Loop A\" x [prompt=\"p\"] } }");
        let Statement::Subgraph(sub) = &g.statements[0] else {
            panic!("a subgraph");
        };
        assert_eq!(
            sub.name.as_ref().map(|n| n.name.as_str()),
            Some("cluster_a")
        );
        assert_eq!(sub.statements.len(), 2);
    }

    #[test]
    fn unsupported_constructs_have_specific_codes() {
        let code = |text: &str| parse("w", text).expect_err("rejected").code.to_string();
        assert_eq!(code("strict digraph G {}"), "unsupported.dot.strict");
        assert_eq!(code("graph G { a -- b }"), "unsupported.dot.undirected");
        assert_eq!(code("digraph G { a:p -> b }"), "unsupported.dot.port");
        assert_eq!(
            code("digraph G { { a } }"),
            "unsupported.dot.anonymous_subgraph"
        );
        assert_eq!(
            code("digraph G { a -> { b } }"),
            "unsupported.dot.subgraph_edge"
        );
        assert_eq!(
            code("digraph G { a [label=<b>] }"),
            "unsupported.dot.html_string"
        );
        assert_eq!(code("digraph G { a -> }"), "dot.syntax");
        assert_eq!(code("digraph G { a"), "dot.syntax");
    }
}
