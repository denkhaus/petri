//! Expressions: a small, total, side-effect-free language evaluated in the pure
//! core.
//!
//! Expressions are stored flat in an [`ExprTable`] and referenced by
//! [`ExprId`], so a [`Graph`](crate::Graph) stays a plain tree of `Copy` ids
//! with no interior pointers. Evaluation has no IO, no clocks and no
//! randomness: the same [`StaticCtx`] always produces the same [`Value`].
//!
//! Evaluation is **total**: a missing field is `null` rather than an error, so
//! a guard always yields a boolean and a typo can never fail a run at the wrong
//! moment. The cost is that a typo is silently falsy instead. Reserved seam for
//! v2: a strict mode, or an unknown-field lint at load time, that reports a
//! path no context can ever bind.

pub mod builtins;
mod eval;
mod table;

pub use builtins::{BUILTINS, Builtin, builtin};
pub use eval::{EvalEnv, EvalError, StaticCtx, eval, eval_bool, is_truthy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;
pub use table::ExprTable;

use crate::ids::{ExprId, Live};

/// One expression node. Sub-expressions are referenced by [`ExprId`], in the
/// same id space `S` as the table that holds this expression.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr<S = Live> {
    /// A literal JSON value.
    Lit(Value),
    /// A binding looked up in the [`StaticCtx`] (`outcome`, `output`, `item`,
    /// `env`, ...).
    Var(SmolStr),
    /// Field access. Missing fields evaluate to `null` rather than erroring.
    Field(ExprId<S>, SmolStr),
    /// Index access on an array (by number) or object (by string key).
    Index(ExprId<S>, ExprId<S>),
    Unary(UnOp, ExprId<S>),
    Binary(BinOp, ExprId<S>, ExprId<S>),
    /// `cond ? then : otherwise`, with lazy branches.
    Cond {
        cond:      ExprId<S>,
        then:      ExprId<S>,
        otherwise: ExprId<S>,
    },
    Array(Vec<ExprId<S>>),
    Object(Vec<(SmolStr, ExprId<S>)>),
    Call(SmolStr, Vec<ExprId<S>>),
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
