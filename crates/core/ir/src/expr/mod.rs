//! Expressions: a small, total, side-effect-free language evaluated in the pure core.
//!
//! Expressions are stored flat in an [`ExprTable`] and referenced by [`ExprId`], so a
//! [`Graph`](crate::Graph) stays a plain tree of `Copy` ids with no interior pointers.
//! Evaluation has no IO, no clocks and no randomness: the same [`StaticCtx`] always
//! produces the same [`Value`].
//!
//! Evaluation is **total**: a missing field is `null` rather than an error, so a
//! guard always yields a boolean and a typo can never fail a run at the wrong
//! moment. The cost is that a typo is silently falsy instead. Reserved seam for v2:
//! a strict mode, or an unknown-field lint at load time, that reports a path no
//! context can ever bind.

pub mod builtins;
mod eval;
mod table;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::ids::ExprId;

pub use builtins::{BUILTINS, Builtin, builtin};
pub use eval::{EvalEnv, EvalError, StaticCtx, eval, eval_bool, truthy};
pub use table::ExprTable;

/// One expression node. Sub-expressions are referenced by [`ExprId`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// A literal JSON value.
    Lit(Value),
    /// A binding looked up in the [`StaticCtx`] (`outcome`, `output`, `item`, `env`, ...).
    Var(SmolStr),
    /// Field access. Missing fields evaluate to `null` rather than erroring.
    Field(ExprId, SmolStr),
    /// Index access on an array (by number) or object (by string key).
    Index(ExprId, ExprId),
    Unary(UnOp, ExprId),
    Binary(BinOp, ExprId, ExprId),
    /// `cond ? then : otherwise`, with lazy branches.
    Cond {
        cond: ExprId,
        then: ExprId,
        otherwise: ExprId,
    },
    Array(Vec<ExprId>),
    Object(Vec<(SmolStr, ExprId)>),
    Call(SmolStr, Vec<ExprId>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// Array/string concatenation (`acc ++ [output]`).
    Concat,
}
