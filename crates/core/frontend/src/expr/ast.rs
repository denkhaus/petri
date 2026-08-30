//! The expression syntax tree.

use std::fmt;

use super::print;

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
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }

    /// Binding strength; higher binds tighter.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::Ne => 3,
            Self::Lt | Self::Le | Self::Gt | Self::Ge => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// A bare identifier: a context name (`github`) or, in the native format,
    /// an engine binding (`nodes`, `item`).
    Ident(String),
    /// `a.b` — property access by name.
    Property(Box<Self>, String),
    /// `a[expr]` — index by a computed key.
    Index(Box<Self>, Box<Self>),
    /// `a.*` or `a[*]` — the object filter: an object's values, or an array's
    /// elements.
    Wildcard(Box<Self>),
    Unary(UnaryOp, Box<Self>),
    Binary(BinaryOp, Box<Self>, Box<Self>),
    Call(String, Vec<Self>),
    /// `(expr)`, kept so the printer round-trips exactly and precedence is
    /// explicit.
    Group(Box<Self>),
}

impl Expr {
    pub fn null() -> Self {
        Self::Literal(Literal::Null)
    }

    pub fn boolean(b: bool) -> Self {
        Self::Literal(Literal::Bool(b))
    }

    pub fn string(s: &str) -> Self {
        Self::Literal(Literal::Str(s.to_string()))
    }

    pub fn number(n: f64) -> Self {
        Self::Literal(Literal::Number(n))
    }

    pub fn ident(name: &str) -> Self {
        Self::Ident(name.to_string())
    }

    #[must_use]
    pub fn property(self, name: &str) -> Self {
        Self::Property(Box::new(self), name.to_string())
    }

    pub fn call(name: &str, args: Vec<Self>) -> Self {
        Self::Call(name.to_string(), args)
    }

    /// The chain of property names from a root identifier: `a.b.c` → `("a", [b,
    /// c])`. `None` when the expression is not a plain dotted path.
    pub fn dotted_path(&self) -> Option<(&str, Vec<&str>)> {
        match self {
            Self::Ident(name) => Some((name, Vec::new())),
            Self::Property(base, name) => {
                let (root, mut path) = base.dotted_path()?;
                path.push(name);
                Some((root, path))
            }
            Self::Index(base, key) => {
                let Self::Literal(Literal::Str(key)) = key.as_ref() else {
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
            Self::Ident(name) => Some(name),
            Self::Property(base, _) | Self::Index(base, _) | Self::Wildcard(base) => {
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
            Self::Literal(_) => {}
            Self::Ident(name) => out.push(name),
            Self::Property(base, _) | Self::Wildcard(base) | Self::Group(base) => {
                base.collect_roots(out);
            }
            Self::Index(base, key) => {
                base.collect_roots(out);
                key.collect_roots(out);
            }
            Self::Unary(_, inner) => inner.collect_roots(out),
            Self::Binary(_, l, r) => {
                l.collect_roots(out);
                r.collect_roots(out);
            }
            Self::Call(_, args) => {
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
            Self::Literal(_) | Self::Ident(_) => {}
            Self::Property(base, _) | Self::Wildcard(base) | Self::Group(base) => {
                base.collect_calls(out);
            }
            Self::Index(base, key) => {
                base.collect_calls(out);
                key.collect_calls(out);
            }
            Self::Unary(_, inner) => inner.collect_calls(out),
            Self::Binary(_, l, r) => {
                l.collect_calls(out);
                r.collect_calls(out);
            }
            Self::Call(name, args) => {
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
        f.write_str(&print::print(self))
    }
}
