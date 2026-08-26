//! Lowering the syntax tree onto the engine's expression table.
//!
//! Two lowerings share one parser. Neither evaluates: they build `ir::Expr` nodes,
//! and every function they emit is looked up in `ir::expr::BUILTINS` first, so a
//! lowering cannot produce a call the engine will not take.

use ir::{BinOp, ExprId, ExprTable, UnOp};
use serde_json::Value;

use super::ast::{BinaryOp, Expr, Literal, UnaryOp};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LowerError {
    #[error("unknown function `{0}`")]
    UnknownFunction(String),
    #[error("`{name}` takes {expected} argument(s), got {got}")]
    Arity {
        name: String,
        expected: usize,
        got: usize,
    },
    #[error("`{0}` is not a binding this format knows")]
    UnknownIdent(String),
    #[error("{0}")]
    Custom(String),
}

/// Resolve a root identifier to an engine expression. How `github` or `steps` or
/// `item` becomes an `ExprId` is the frontend's business; the lowering asks.
pub trait Roots {
    /// Lower a bare identifier. `None` means "not a name I know".
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId>;

    /// A dotted path from a root, for frontends that resolve some paths
    /// structurally — `steps.build.outputs.x` becomes a run-context lookup rather
    /// than field access on a `steps` object. `None` means "resolve it the ordinary
    /// way": segment by segment, with the lowering's own property semantics.
    fn path(&mut self, _root: &str, _path: &[&str], _table: &mut ExprTable) -> Option<ExprId> {
        None
    }

    /// Lower a function call the frontend wants to intercept — GitHub's status
    /// functions, say, which depend on where the expression sits. `None` means "use
    /// the builtin of the same name".
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

fn literal(table: &mut ExprTable, lit: &Literal) -> ExprId {
    match lit {
        Literal::Null => table.lit(Value::Null),
        Literal::Bool(b) => table.lit(*b),
        Literal::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                table.lit(*n as i64)
            } else {
                table.lit(serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number))
            }
        }
        Literal::Str(s) => table.lit(s.as_str()),
    }
}

fn check_builtin(name: &str, got: usize) -> Result<(), LowerError> {
    let spec =
        ir::expr::builtin(name).ok_or_else(|| LowerError::UnknownFunction(name.to_string()))?;
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

/// Lower with the engine's semantics: `==` is `Eq`, `&&` is `And`, `!` is `Not`,
/// functions are builtins by name. For the native format.
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

// ── GHA: GitHub's semantics, on the same tree ─────────────────────────────

/// Lower with GitHub's semantics. Operators become `loose_*` builtins; `&&` and `||`
/// return operand values; `*` and the filtered-array rule follow `crate::loose`.
///
/// Function names are GitHub's, mapped onto builtins by [`gha_function`]. Contexts
/// are resolved by `roots`, which is where a frontend decides what `steps.x` means.
pub fn gha(
    expr: &Expr,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<ExprId, LowerError> {
    Ok(gha_inner(expr, table, roots)?.id)
}

/// A lowered sub-expression, with whether it is a GitHub "filtered array" — the
/// result of `*`, on which a later property access maps over elements.
struct Lowered {
    id: ExprId,
    filtered: bool,
}

fn gha_inner(
    expr: &Expr,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<Lowered, LowerError> {
    let plain = |id: ExprId| Lowered {
        id,
        filtered: false,
    };
    match expr {
        Expr::Literal(lit) => Ok(plain(literal(table, lit))),
        Expr::Ident(name) => roots
            .root(name, table)
            .map(plain)
            .ok_or_else(|| LowerError::UnknownIdent(name.clone())),
        // A dotted path is handed to the frontend whole, so `steps.build.outputs.x`
        // can resolve structurally. Anything it declines falls through.
        Expr::Property(..) | Expr::Index(..) if expr.dotted_path().is_some() => {
            let (root, path) = expr.dotted_path().expect("checked");
            if let Some(id) = roots.path(root, &path, table) {
                return Ok(plain(id));
            }
            match expr {
                Expr::Property(base, name) => gha_property(base, name, table, roots),
                Expr::Index(base, key) => gha_index(base, key, table, roots),
                _ => unreachable!(),
            }
        }
        Expr::Property(base, name) => gha_property(base, name, table, roots),
        Expr::Index(base, key) => gha_index(base, key, table, roots),
        Expr::Wildcard(base) => {
            let base = gha_inner(base, table, roots)?;
            // `*` on a filtered array flattens one level; on an object takes its
            // values; on an array is the array. `values` does the last two, and a
            // filtered array is already an array.
            let id = builtin(table, "values", vec![base.id])?;
            Ok(Lowered { id, filtered: true })
        }
        Expr::Unary(UnaryOp::Not, inner) => {
            let inner = gha_inner(inner, table, roots)?;
            let truthy = builtin(table, "loose_truthy", vec![inner.id])?;
            Ok(plain(table.unary(UnOp::Not, truthy)))
        }
        Expr::Binary(op, l, r) => {
            let l = gha_inner(l, table, roots)?.id;
            let r = gha_inner(r, table, roots)?.id;
            let id = match op {
                BinaryOp::Lt => builtin(table, "loose_lt", vec![l, r])?,
                BinaryOp::Le => builtin(table, "loose_le", vec![l, r])?,
                BinaryOp::Gt => builtin(table, "loose_gt", vec![l, r])?,
                BinaryOp::Ge => builtin(table, "loose_ge", vec![l, r])?,
                BinaryOp::Eq => builtin(table, "loose_eq", vec![l, r])?,
                BinaryOp::Ne => {
                    let eq = builtin(table, "loose_eq", vec![l, r])?;
                    table.unary(UnOp::Not, eq)
                }
                // `a && b` is `b` when `a` is truthy, else `a` — the operand, not a
                // boolean. Likewise `a || b` is `a` when truthy, else `b`.
                BinaryOp::And => {
                    let truthy = builtin(table, "loose_truthy", vec![l])?;
                    table.cond(truthy, r, l)
                }
                BinaryOp::Or => {
                    let truthy = builtin(table, "loose_truthy", vec![l])?;
                    table.cond(truthy, l, r)
                }
            };
            Ok(plain(id))
        }
        Expr::Call(name, args) => {
            if let Some(intercepted) = roots.call(name, args, table) {
                return intercepted.map(plain);
            }
            let mut ids = Vec::with_capacity(args.len());
            for a in args {
                ids.push(gha_inner(a, table, roots)?.id);
            }
            gha_function(name, ids, table).map(plain)
        }
        Expr::Group(inner) => gha_inner(inner, table, roots),
    }
}

fn gha_property(
    base: &Expr,
    name: &str,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<Lowered, LowerError> {
    let base = gha_inner(base, table, roots)?;
    let key = table.lit(name);
    if base.filtered {
        // Property access on a filtered array maps over its elements, keeping the
        // result filtered so the chain continues.
        let id = builtin(table, "filter_field", vec![base.id, key])?;
        return Ok(Lowered { id, filtered: true });
    }
    // Contexts are case-insensitive, so a plain property lookup is too.
    let id = builtin(table, "get_ci", vec![base.id, key])?;
    Ok(Lowered {
        id,
        filtered: false,
    })
}

fn gha_index(
    base: &Expr,
    key: &Expr,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<Lowered, LowerError> {
    let base = gha_inner(base, table, roots)?;
    let key_id = gha_inner(key, table, roots)?.id;
    if base.filtered {
        // Only string keys map over a filtered array; a numeric index into a filtered
        // array is the element, which `Index` already does.
        if matches!(key, Expr::Literal(Literal::Str(_))) {
            let id = builtin(table, "filter_field", vec![base.id, key_id])?;
            return Ok(Lowered { id, filtered: true });
        }
        return Ok(Lowered {
            id: table.index(base.id, key_id),
            filtered: false,
        });
    }
    // A string key is a (case-insensitive) property; a number is an array index.
    // When the key is computed, `Index` handles both shapes.
    let id = match key {
        Expr::Literal(Literal::Str(_)) => builtin(table, "get_ci", vec![base.id, key_id])?,
        _ => table.index(base.id, key_id),
    };
    Ok(Lowered {
        id,
        filtered: false,
    })
}

/// GitHub's function names, mapped onto the builtin table.
///
/// The names are the compatibility surface; the semantics live in the engine. A
/// GitHub function with no faithful builtin is a finding, not a special case here —
/// `hashFiles` is rejected by the frontend before this point because it reads the
/// workspace, which no total builtin can.
pub fn gha_function(
    name: &str,
    args: Vec<ExprId>,
    table: &mut ExprTable,
) -> Result<ExprId, LowerError> {
    let arity = |expected: usize| -> Result<(), LowerError> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(LowerError::Arity {
                name: name.to_string(),
                expected,
                got: args.len(),
            })
        }
    };
    // Function names are case-insensitive in GitHub.
    match name.to_lowercase().as_str() {
        "contains" => {
            arity(2)?;
            builtin(table, "contains_ci", args)
        }
        "startswith" => {
            arity(2)?;
            builtin(table, "starts_with", args)
        }
        "endswith" => {
            arity(2)?;
            builtin(table, "ends_with", args)
        }
        "format" => {
            if args.is_empty() {
                return Err(LowerError::Arity {
                    name: name.to_string(),
                    expected: 1,
                    got: 0,
                });
            }
            let mut args = args;
            let template = args.remove(0);
            let rest = table.array(args);
            builtin(table, "format", vec![template, rest])
        }
        "join" => {
            if args.is_empty() || args.len() > 2 {
                return Err(LowerError::Arity {
                    name: name.to_string(),
                    expected: 2,
                    got: args.len(),
                });
            }
            let mut args = args;
            let items = args.remove(0);
            let separator = match args.pop() {
                Some(sep) => sep,
                None => table.lit(Value::Null),
            };
            builtin(table, "join", vec![items, separator])
        }
        "tojson" => {
            arity(1)?;
            builtin(table, "to_json", args)
        }
        "fromjson" => {
            arity(1)?;
            builtin(table, "from_json", args)
        }
        "always" | "success" | "failure" | "cancelled" => {
            // Status functions depend on where the expression sits; a frontend that
            // does not intercept them gets the engine's upstream-fold semantics.
            arity(0)?;
            builtin(table, &name.to_lowercase(), args)
        }
        other => Err(LowerError::UnknownFunction(other.to_string())),
    }
}

/// The GitHub function names this lowering understands, for the "every function has
/// a home" test and for diagnostics.
pub const GHA_FUNCTIONS: &[&str] = &[
    "contains",
    "startsWith",
    "endsWith",
    "format",
    "join",
    "toJSON",
    "fromJSON",
    "always",
    "success",
    "failure",
    "cancelled",
];

/// GitHub functions the engine cannot express as a total builtin, with why.
pub const GHA_FUNCTIONS_UNSUPPORTED: &[(&str, &str)] = &[(
    "hashFiles",
    "reads the workspace at run time, which no pure builtin can; it needs a resolution-time placeholder like `$secret`",
)];
