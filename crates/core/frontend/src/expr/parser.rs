//! A recursive-descent parser for the `${{ }}` grammar.
//!
//! Precedence, loosest to tightest: `||`, `&&`, `== !=`, `< <= > >=`, unary `!`,
//! then postfix `.name`, `[expr]`, `.*`, and calls. Comparison operators are
//! non-associative in GitHub (`a < b < c` is an error); this parser follows that.

use super::ast::{BinaryOp, Expr, Literal, UnaryOp};
use super::lexer::{LexError, Token, TokenKind, lex};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("{0}")]
    Lex(LexError),
    #[error("expected {expected}, found {found}")]
    Unexpected {
        expected: String,
        found: String,
        offset: usize,
    },
    #[error("comparison operators do not chain; parenthesize one side")]
    ChainedComparison { offset: usize },
    #[error("empty expression")]
    Empty,
    #[error("expression nests too deeply")]
    TooDeep { offset: usize },
}

impl ParseError {
    /// Byte offset the error points at, within the expression source.
    pub fn offset(&self) -> usize {
        match self {
            ParseError::Lex(e) => e.offset(),
            ParseError::Unexpected { offset, .. }
            | ParseError::ChainedComparison { offset }
            | ParseError::TooDeep { offset } => *offset,
            ParseError::Empty => 0,
        }
    }
}

const MAX_DEPTH: usize = 128;

pub fn parse(source: &str) -> Result<Expr, ParseError> {
    let tokens = lex(source).map_err(ParseError::Lex)?;
    if matches!(tokens.first().map(|t| &t.kind), Some(TokenKind::Eof) | None) {
        return Err(ParseError::Empty);
    }
    let mut p = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    let expr = p.expression()?;
    p.expect(&TokenKind::Eof, "end of expression")?;
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn advance(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, kind: &TokenKind) -> bool {
        &self.peek().kind == kind
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Token, ParseError> {
        if self.at(kind) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        let t = self.peek();
        ParseError::Unexpected {
            expected: expected.to_string(),
            found: describe(&t.kind),
            offset: t.offset,
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError::TooDeep {
                offset: self.peek().offset,
            });
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn expression(&mut self) -> Result<Expr, ParseError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and()?;
        while self.at(&TokenKind::Or) {
            self.advance();
            let right = self.and()?;
            left = Expr::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.equality()?;
        while self.at(&TokenKind::And) {
            self.advance();
            let right = self.equality()?;
            left = Expr::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.comparison()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Eq => BinaryOp::Eq,
                TokenKind::Ne => BinaryOp::Ne,
                _ => break,
            };
            self.advance();
            let right = self.comparison()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expr, ParseError> {
        let left = self.unary()?;
        let op = match self.peek().kind {
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::Le => BinaryOp::Le,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::Ge => BinaryOp::Ge,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.unary()?;
        if matches!(
            self.peek().kind,
            TokenKind::Lt | TokenKind::Le | TokenKind::Gt | TokenKind::Ge
        ) {
            return Err(ParseError::ChainedComparison {
                offset: self.peek().offset,
            });
        }
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.at(&TokenKind::Not) {
            self.advance();
            self.enter()?;
            let inner = self.unary()?;
            self.leave();
            return Ok(Expr::Unary(UnaryOp::Not, Box::new(inner)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.primary()?;
        loop {
            match self.peek().kind {
                TokenKind::Dot => {
                    self.advance();
                    match self.advance().kind {
                        TokenKind::Star => expr = Expr::Wildcard(Box::new(expr)),
                        TokenKind::Ident(name) => expr = Expr::Property(Box::new(expr), name),
                        // Keywords are valid property names after a dot.
                        TokenKind::Null => expr = Expr::Property(Box::new(expr), "null".into()),
                        TokenKind::True => expr = Expr::Property(Box::new(expr), "true".into()),
                        TokenKind::False => expr = Expr::Property(Box::new(expr), "false".into()),
                        _ => {
                            self.pos -= 1;
                            return Err(self.unexpected("a property name or `*`"));
                        }
                    }
                }
                TokenKind::LBracket => {
                    self.advance();
                    if self.at(&TokenKind::Star) {
                        self.advance();
                        self.expect(&TokenKind::RBracket, "`]`")?;
                        expr = Expr::Wildcard(Box::new(expr));
                    } else {
                        self.enter()?;
                        let key = self.expression()?;
                        self.leave();
                        self.expect(&TokenKind::RBracket, "`]`")?;
                        expr = Expr::Index(Box::new(expr), Box::new(key));
                    }
                }
                _ => return Ok(expr),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let token = self.advance();
        match token.kind {
            TokenKind::Null => Ok(Expr::Literal(Literal::Null)),
            TokenKind::True => Ok(Expr::Literal(Literal::Bool(true))),
            TokenKind::False => Ok(Expr::Literal(Literal::Bool(false))),
            TokenKind::Number(n) => Ok(Expr::Literal(Literal::Number(n))),
            TokenKind::Str(s) => Ok(Expr::Literal(Literal::Str(s))),
            TokenKind::LParen => {
                self.enter()?;
                let inner = self.expression()?;
                self.leave();
                self.expect(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Group(Box::new(inner)))
            }
            TokenKind::Ident(name) => {
                if self.at(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.at(&TokenKind::RParen) {
                        loop {
                            self.enter()?;
                            args.push(self.expression()?);
                            self.leave();
                            if self.at(&TokenKind::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(&TokenKind::RParen, "`)`")?;
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            _ => {
                self.pos -= 1;
                Err(self.unexpected("a value"))
            }
        }
    }
}

fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Eof => "end of expression".to_string(),
        TokenKind::Ident(name) => format!("`{name}`"),
        TokenKind::Str(s) => format!("'{s}'"),
        TokenKind::Number(n) => format!("{n}"),
        TokenKind::Null => "`null`".into(),
        TokenKind::True => "`true`".into(),
        TokenKind::False => "`false`".into(),
        TokenKind::Dot => "`.`".into(),
        TokenKind::Star => "`*`".into(),
        TokenKind::Comma => "`,`".into(),
        TokenKind::LParen => "`(`".into(),
        TokenKind::RParen => "`)`".into(),
        TokenKind::LBracket => "`[`".into(),
        TokenKind::RBracket => "`]`".into(),
        TokenKind::Not => "`!`".into(),
        TokenKind::Lt => "`<`".into(),
        TokenKind::Le => "`<=`".into(),
        TokenKind::Gt => "`>`".into(),
        TokenKind::Ge => "`>=`".into(),
        TokenKind::Eq => "`==`".into(),
        TokenKind::Ne => "`!=`".into(),
        TokenKind::And => "`&&`".into(),
        TokenKind::Or => "`||`".into(),
    }
}
