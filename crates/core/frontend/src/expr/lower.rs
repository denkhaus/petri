//! Lowering the syntax tree onto the engine's expression table.
//!
//! This crate ships one lowering, [`strict`]: the engine's own semantics, for
//! the native format. A frontend with its own semantics (GitHub's loose
//! comparison, say) writes its own lowering in its own crate against the same
//! [`Roots`] trait and the same [`builtin`] gate. Nothing here evaluates: a
//! lowering builds `ir::Expr` nodes, and every function it emits is looked up
//! in `ir::expr::BUILTINS` first, so it cannot produce a call the engine will
//! not take.

use ir::{BinOp, ExprId, ExprTable, UnOp, expr as ir_expr};
use serde_json::Value;

use super::ast::{BinaryOp, Expr, Literal, UnaryOp};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LowerError {
    #[error("unknown function `{0}`")]
    UnknownFunction(String),
    #[error("`{name}` takes {expected} argument(s), got {got}")]
    Arity {
        name:     String,
        expected: usize,
        got:      usize,
    },
    #[error("`{0}` is not a binding this format knows")]
    UnknownIdent(String),
    #[error("{0}")]
    Custom(String),
}

/// Resolve a root identifier to an engine expression. How `github` or `steps`
/// or `item` becomes an `ExprId` is the frontend's business; the lowering asks.
pub trait Roots {
    /// Lower a bare identifier. `None` means "not a name I know".
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId>;

    /// A dotted path from a root, for frontends that resolve some paths
    /// structurally — `steps.build.outputs.x` becomes a run-context lookup
    /// rather than field access on a `steps` object. `None` means "resolve
    /// it the ordinary way": segment by segment, with the lowering's own
    /// property semantics.
    fn path(&mut self, _root: &str, _path: &[&str], _table: &mut ExprTable) -> Option<ExprId> {
        None
    }

    /// Lower a function call the frontend wants to intercept — GitHub's status
    /// functions, say, which depend on where the expression sits. `None` means
    /// "use the builtin of the same name".
    fn call(
        &mut self,
        _name: &str,
        _args: &[Expr],
        _table: &mut ExprTable,
    ) -> Option<Result<ExprId, LowerError>> {
        None
    }
}

/// Every identifier is an engine binding, taken as written.
pub struct EngineBindings;

impl Roots for EngineBindings {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        Some(table.var(name))
    }
}

/// Lower a literal. Shared by every lowering.
pub fn literal(table: &mut ExprTable, lit: &Literal) -> ExprId {
    let value = literal_value(lit);
    table.lit(value)
}

/// A literal's value — the one rule for how a parsed literal (integral numbers
/// included) becomes JSON, shared by every lowering.
pub fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Bool(b) => Value::Bool(*b),
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the guard proves the number is integral and inside i64's range"
        )]
        Literal::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                Value::from(*n as i64)
            } else {
                serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number)
            }
        }
        Literal::Str(s) => Value::String(s.clone()),
    }
}

fn check_builtin(name: &str, got: usize) -> Result<(), LowerError> {
    let spec =
        ir_expr::builtin(name).ok_or_else(|| LowerError::UnknownFunction(name.to_string()))?;
    if spec.arity != got {
        return Err(LowerError::Arity {
            name: name.to_string(),
            expected: spec.arity,
            got,
        });
    }
    Ok(())
}

/// Call a builtin, checking it exists and takes this many arguments.
pub fn builtin(table: &mut ExprTable, name: &str, args: Vec<ExprId>) -> Result<ExprId, LowerError> {
    check_builtin(name, args.len())?;
    Ok(table.call(name, args))
}

// ── Strict: the engine's own semantics ────────────────────────────────────

/// Lower with the engine's semantics: `==` is `Eq`, `&&` is `And`, `!` is
/// `Not`, functions are builtins by name. For the native format.
pub fn strict(
    expr: &Expr,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<ExprId, LowerError> {
    match expr {
        Expr::Literal(lit) => Ok(literal(table, lit)),
        Expr::Ident(name) => roots
            .root(name, table)
            .ok_or_else(|| LowerError::UnknownIdent(name.clone())),
        Expr::Property(..) | Expr::Index(..) if expr.dotted_path().is_some() => {
            let (root, path) = expr.dotted_path().expect("checked");
            if let Some(id) = roots.path(root, &path, table) {
                return Ok(id);
            }
            match expr {
                Expr::Property(base, name) => {
                    let base = strict(base, table, roots)?;
                    Ok(table.field(base, name))
                }
                Expr::Index(base, key) => {
                    let base = strict(base, table, roots)?;
                    let key = strict(key, table, roots)?;
                    Ok(table.index(base, key))
                }
                _ => unreachable!(),
            }
        }
        Expr::Property(base, name) => {
            let base = strict(base, table, roots)?;
            Ok(table.field(base, name))
        }
        Expr::Index(base, key) => {
            let base = strict(base, table, roots)?;
            let key = strict(key, table, roots)?;
            Ok(table.index(base, key))
        }
        Expr::Wildcard(base) => {
            let base = strict(base, table, roots)?;
            builtin(table, "values", vec![base])
        }
        Expr::Unary(UnaryOp::Not, inner) => {
            let inner = strict(inner, table, roots)?;
            Ok(table.unary(UnOp::Not, inner))
        }
        Expr::Binary(op, l, r) => {
            let l = strict(l, table, roots)?;
            let r = strict(r, table, roots)?;
            let op = match op {
                BinaryOp::Lt => BinOp::Lt,
                BinaryOp::Le => BinOp::Le,
                BinaryOp::Gt => BinOp::Gt,
                BinaryOp::Ge => BinOp::Ge,
                BinaryOp::Eq => BinOp::Eq,
                BinaryOp::Ne => BinOp::Ne,
                BinaryOp::And => BinOp::And,
                BinaryOp::Or => BinOp::Or,
            };
            Ok(table.binary(op, l, r))
        }
        Expr::Call(name, args) => {
            if let Some(intercepted) = roots.call(name, args, table) {
                return intercepted;
            }
            let mut ids = Vec::with_capacity(args.len());
            for a in args {
                ids.push(strict(a, table, roots)?);
            }
            builtin(table, name, ids)
        }
        Expr::Group(inner) => strict(inner, table, roots),
    }
}
