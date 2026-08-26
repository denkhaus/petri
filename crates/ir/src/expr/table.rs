//! The flat expression store, and the construction helpers frontends build with.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use super::{BinOp, Expr, UnOp};
use crate::ids::ExprId;

/// Flat store of every expression in a graph.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExprTable {
    exprs: Vec<Expr>,
}

impl ExprTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, expr: Expr) -> ExprId {
        let id = ExprId::new(u32::try_from(self.exprs.len()).expect("expression table overflow"));
        self.exprs.push(expr);
        id
    }

    pub fn get(&self, id: ExprId) -> Option<&Expr> {
        self.exprs.get(id.index())
    }

    pub fn len(&self) -> usize {
        self.exprs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ExprId, &Expr)> {
        self.exprs
            .iter()
            .enumerate()
            .map(|(i, e)| (ExprId::new(i as u32), e))
    }

    // ── Construction helpers ──────────────────────────────────────────────
    // Frontends build expressions through these; they read close to the source
    // syntax they lower from.

    pub fn lit(&mut self, v: impl Into<Value>) -> ExprId {
        self.push(Expr::Lit(v.into()))
    }

    pub fn var(&mut self, name: &str) -> ExprId {
        self.push(Expr::Var(SmolStr::new(name)))
    }

    /// Dotted path over a binding: `path("outcome", ["status"])`.
    pub fn path(&mut self, root: &str, fields: &[&str]) -> ExprId {
        let mut id = self.var(root);
        for f in fields {
            id = self.push(Expr::Field(id, SmolStr::new(*f)));
        }
        id
    }

    pub fn field(&mut self, base: ExprId, name: &str) -> ExprId {
        self.push(Expr::Field(base, SmolStr::new(name)))
    }

    pub fn index(&mut self, base: ExprId, idx: ExprId) -> ExprId {
        self.push(Expr::Index(base, idx))
    }

    pub fn binary(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
        self.push(Expr::Binary(op, lhs, rhs))
    }

    pub fn unary(&mut self, op: UnOp, arg: ExprId) -> ExprId {
        self.push(Expr::Unary(op, arg))
    }

    pub fn cond(&mut self, cond: ExprId, then: ExprId, otherwise: ExprId) -> ExprId {
        self.push(Expr::Cond {
            cond,
            then,
            otherwise,
        })
    }

    pub fn array(&mut self, items: Vec<ExprId>) -> ExprId {
        self.push(Expr::Array(items))
    }

    pub fn object(&mut self, fields: Vec<(&str, ExprId)>) -> ExprId {
        self.push(Expr::Object(
            fields
                .into_iter()
                .map(|(k, v)| (SmolStr::new(k), v))
                .collect(),
        ))
    }

    pub fn call(&mut self, name: &str, args: Vec<ExprId>) -> ExprId {
        self.push(Expr::Call(SmolStr::new(name), args))
    }
}
