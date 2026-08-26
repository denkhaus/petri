//! The expression syntax tree.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    /// Numbers are doubles.
    Number(f64),
    Str(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        }
    }

    /// Binding strength; higher binds tighter.
    pub fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => 1,
            BinaryOp::And => 2,
            BinaryOp::Eq | BinaryOp::Ne => 3,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// A bare identifier: a context name (`github`) or, in the native format, an
    /// engine binding (`nodes`, `item`).
    Ident(String),
    /// `a.b` — property access by name.
    Property(Box<Expr>, String),
    /// `a[expr]` — index by a computed key.
    Index(Box<Expr>, Box<Expr>),
    /// `a.*` or `a[*]` — the object filter: an object's values, or an array's elements.
    Wildcard(Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    /// `(expr)`, kept so the printer round-trips exactly and precedence is explicit.
    Group(Box<Expr>),
}

impl Expr {
    pub fn null() -> Self {
        Expr::Literal(Literal::Null)
    }

    pub fn boolean(b: bool) -> Self {
        Expr::Literal(Literal::Bool(b))
    }

    pub fn string(s: &str) -> Self {
        Expr::Literal(Literal::Str(s.to_string()))
    }

    pub fn number(n: f64) -> Self {
        Expr::Literal(Literal::Number(n))
    }

    pub fn ident(name: &str) -> Self {
        Expr::Ident(name.to_string())
    }

    pub fn property(self, name: &str) -> Self {
        Expr::Property(Box::new(self), name.to_string())
    }

    pub fn call(name: &str, args: Vec<Expr>) -> Self {
        Expr::Call(name.to_string(), args)
    }

    /// The chain of property names from a root identifier: `a.b.c` → `("a", [b, c])`.
    /// `None` when the expression is not a plain dotted path.
    pub fn dotted_path(&self) -> Option<(&str, Vec<&str>)> {
        match self {
            Expr::Ident(name) => Some((name, Vec::new())),
            Expr::Property(base, name) => {
                let (root, mut path) = base.dotted_path()?;
                path.push(name);
                Some((root, path))
            }
            Expr::Index(base, key) => {
                let Expr::Literal(Literal::Str(key)) = key.as_ref() else {
                    return None;
                };
                let (root, mut path) = base.dotted_path()?;
                path.push(key);
                Some((root, path))
            }
            _ => None,
        }
    }

    /// The root identifier of a property/index chain, if any.
    pub fn root_ident(&self) -> Option<&str> {
        match self {
            Expr::Ident(name) => Some(name),
            Expr::Property(base, _) | Expr::Index(base, _) | Expr::Wildcard(base) => {
                base.root_ident()
            }
            _ => None,
        }
    }

    /// Every identifier root referenced anywhere in the expression.
    pub fn roots(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_roots(&mut out);
        out
    }

    fn collect_roots<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Literal(_) => {}
            Expr::Ident(name) => out.push(name),
            Expr::Property(base, _) | Expr::Wildcard(base) | Expr::Group(base) => {
                base.collect_roots(out)
            }
            Expr::Index(base, key) => {
                base.collect_roots(out);
                key.collect_roots(out);
            }
            Expr::Unary(_, inner) => inner.collect_roots(out),
            Expr::Binary(_, l, r) => {
                l.collect_roots(out);
                r.collect_roots(out);
            }
            Expr::Call(_, args) => {
                for a in args {
                    a.collect_roots(out);
                }
            }
        }
    }

    /// Every function name called anywhere in the expression.
    pub fn calls(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_calls(&mut out);
        out
    }

    fn collect_calls<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Literal(_) | Expr::Ident(_) => {}
            Expr::Property(base, _) | Expr::Wildcard(base) | Expr::Group(base) => {
                base.collect_calls(out)
            }
            Expr::Index(base, key) => {
                base.collect_calls(out);
                key.collect_calls(out);
            }
            Expr::Unary(_, inner) => inner.collect_calls(out),
            Expr::Binary(_, l, r) => {
                l.collect_calls(out);
                r.collect_calls(out);
            }
            Expr::Call(name, args) => {
                out.push(name);
                for a in args {
                    a.collect_calls(out);
                }
            }
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::expr::print::print(self))
    }
}
