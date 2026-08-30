//! The flat expression store, and the construction helpers frontends build
//! with.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use super::{BinOp, Expr, UnOp};
use crate::ids::{ExprId, Live};

/// Flat store of every expression in a graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExprTable<S = Live> {
    exprs: Vec<Expr<S>>,
}

impl<S> Default for ExprTable<S> {
    fn default() -> Self {
        Self { exprs: Vec::new() }
    }
}

impl<S> ExprTable<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, expr: Expr<S>) -> ExprId<S> {
        let id = ExprId::new(
            u32::try_from(self.exprs.len())
                .expect("an expression table never exceeds u32::MAX entries"),
        );
        self.exprs.push(expr);
        id
    }

    pub fn get(&self, id: ExprId<S>) -> Option<&Expr<S>> {
        self.exprs.get(id.index())
    }

    pub fn len(&self) -> usize {
        self.exprs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ExprId<S>, &Expr<S>)> {
        self.exprs.iter().enumerate().map(|(i, e)| {
            let id = u32::try_from(i).expect("an expression table never exceeds u32::MAX entries");
            (ExprId::new(id), e)
        })
    }

    // ── Construction helpers ──────────────────────────────────────────────
    // Frontends build expressions through these; they read close to the source
    // syntax they lower from.

    pub fn lit(&mut self, v: impl Into<Value>) -> ExprId<S> {
        self.push(Expr::Lit(v.into()))
    }

    pub fn var(&mut self, name: &str) -> ExprId<S> {
        self.push(Expr::Var(SmolStr::new(name)))
    }

    /// Dotted path over a binding: `path("outcome", ["status"])`.
    pub fn path(&mut self, root: &str, fields: &[&str]) -> ExprId<S> {
        let mut id = self.var(root);
        for f in fields {
            id = self.push(Expr::Field(id, SmolStr::new(*f)));
        }
        id
    }

    pub fn field(&mut self, base: ExprId<S>, name: &str) -> ExprId<S> {
        self.push(Expr::Field(base, SmolStr::new(name)))
    }

    pub fn index(&mut self, base: ExprId<S>, idx: ExprId<S>) -> ExprId<S> {
        self.push(Expr::Index(base, idx))
    }

    pub fn binary(&mut self, op: BinOp, lhs: ExprId<S>, rhs: ExprId<S>) -> ExprId<S> {
        self.push(Expr::Binary(op, lhs, rhs))
    }

    pub fn unary(&mut self, op: UnOp, arg: ExprId<S>) -> ExprId<S> {
        self.push(Expr::Unary(op, arg))
    }

    pub fn cond(&mut self, cond: ExprId<S>, then: ExprId<S>, otherwise: ExprId<S>) -> ExprId<S> {
        self.push(Expr::Cond {
            cond,
            then,
            otherwise,
        })
    }

    pub fn array(&mut self, items: Vec<ExprId<S>>) -> ExprId<S> {
        self.push(Expr::Array(items))
    }

    pub fn object(&mut self, fields: Vec<(&str, ExprId<S>)>) -> ExprId<S> {
        self.push(Expr::Object(
            fields
                .into_iter()
                .map(|(k, v)| (SmolStr::new(k), v))
                .collect(),
        ))
    }

    pub fn call(&mut self, name: &str, args: Vec<ExprId<S>>) -> ExprId<S> {
        self.push(Expr::Call(SmolStr::new(name), args))
    }
}
