//! GitHub's semantics, on the shared syntax tree.
//!
//! Operators become `loose_*` builtins; `&&` and `||` return operand values; `*` and
//! the filtered-array rule follow the runner. Function names are GitHub's, mapped onto
//! builtins by [`gha_function`]. Contexts are resolved by the caller's [`Roots`], which
//! is where `steps.x` gets its meaning. Nothing here evaluates.

use frontend::expr::lower::{LowerError, Roots, builtin, literal};
use frontend::expr::{BinaryOp, Expr, Literal, UnaryOp};
use ir::{ExprId, ExprTable, UnOp, Value};

pub fn gha(
    expr: &Expr,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<ExprId, LowerError> {
    Ok(inner(expr, table, roots)?.id)
}

/// A lowered sub-expression, with whether it is a GitHub "filtered array" — the
/// result of `*`, on which a later property access maps over elements. Tracked
/// statically: the parser's shape decides it, so no runtime flag exists.
struct Lowered {
    id: ExprId,
    filtered: bool,
}

fn inner(expr: &Expr, table: &mut ExprTable, roots: &mut dyn Roots) -> Result<Lowered, LowerError> {
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
        // A dotted path is offered to the frontend whole, so `steps.build.outputs.x`
        // can resolve structurally. Anything it declines lowers segment by segment.
        Expr::Property(..) | Expr::Index(..) if expr.dotted_path().is_some() => {
            let (root, path) = expr.dotted_path().expect("checked");
            if let Some(id) = roots.path(root, &path, table) {
                return Ok(plain(id));
            }
            match expr {
                Expr::Property(base, name) => property(base, name, table, roots),
                Expr::Index(base, key) => index(base, key, table, roots),
                _ => unreachable!(),
            }
        }
        Expr::Property(base, name) => property(base, name, table, roots),
        Expr::Index(base, key) => index(base, key, table, roots),
        Expr::Wildcard(base) => {
            let base = inner(base, table, roots)?;
            // `*` on an object takes its values; on an array is the array; on a
            // filtered array flattens one level, which `values` also does.
            let id = builtin(table, "values", vec![base.id])?;
            Ok(Lowered { id, filtered: true })
        }
        Expr::Unary(UnaryOp::Not, arg) => {
            let arg = inner(arg, table, roots)?;
            let truthy = builtin(table, "loose_truthy", vec![arg.id])?;
            Ok(plain(table.unary(UnOp::Not, truthy)))
        }
        Expr::Binary(op, l, r) => {
            let l = inner(l, table, roots)?.id;
            let r = inner(r, table, roots)?.id;
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
                // boolean. `a || b` is `a` when truthy, else `b`.
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
                ids.push(inner(a, table, roots)?.id);
            }
            gha_function(name, ids, table).map(plain)
        }
        Expr::Group(g) => inner(g, table, roots),
    }
}

fn property(
    base: &Expr,
    name: &str,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<Lowered, LowerError> {
    let base = inner(base, table, roots)?;
    let key = table.lit(name);
    if base.filtered {
        // Property access on a filtered array maps over its elements and stays
        // filtered, so the chain continues.
        let id = builtin(table, "pluck_present", vec![base.id, key])?;
        return Ok(Lowered { id, filtered: true });
    }
    // Contexts are case-insensitive, so a plain property lookup is too.
    let id = builtin(table, "get_ci", vec![base.id, key])?;
    Ok(Lowered {
        id,
        filtered: false,
    })
}

fn index(
    base: &Expr,
    key: &Expr,
    table: &mut ExprTable,
    roots: &mut dyn Roots,
) -> Result<Lowered, LowerError> {
    let base = inner(base, table, roots)?;
    let key_id = inner(key, table, roots)?.id;
    if base.filtered {
        // A string key maps over a filtered array; a numeric one indexes it.
        if matches!(key, Expr::Literal(Literal::Str(_))) {
            let id = builtin(table, "pluck_present", vec![base.id, key_id])?;
            return Ok(Lowered { id, filtered: true });
        }
        return Ok(Lowered {
            id: table.index(base.id, key_id),
            filtered: false,
        });
    }
    // A string key is a case-insensitive property; anything else indexes.
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
/// A GitHub function with no faithful builtin is a finding, not a special case here.
/// `hashFiles` is intercepted by the frontend before this point because it reads the
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
            // Site-dependent; a `Roots` that does not intercept them gets the engine's
            // upstream-fold semantics.
            arity(0)?;
            builtin(table, &name.to_lowercase(), args)
        }
        other => Err(LowerError::UnknownFunction(other.to_string())),
    }
}

/// The GitHub function names this lowering understands.
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

/// GitHub's `strategy.matrix`, as a composition of the engine's record combinators.
///
/// The matrix object may be a literal or the result of `fromJSON(...)`; either way
/// the same expression expands it at firing time:
///
/// ```text
/// axes  = omit(matrix, ['include', 'exclude'])
/// legs  = extend_where(reject_where(cartesian(axes), matrix.exclude), matrix.include, keys(axes))
/// ```
///
/// GitHub's rule that an `include` may not overwrite an original axis value is the
/// `keys(axes)` argument: those are the protected keys. Excludes run first, so an
/// include can add a leg back. None of this is in the engine; the engine has
/// combinators, and this is GitHub's use of them.
pub fn matrix_legs(table: &mut ExprTable, matrix: ExprId) -> Result<ExprId, LowerError> {
    let reserved = {
        let inc = table.lit("include");
        let exc = table.lit("exclude");
        table.array(vec![inc, exc])
    };
    let axes = builtin(table, "omit", vec![matrix, reserved])?;
    let product = builtin(table, "cartesian", vec![axes])?;
    let exclude_key = table.lit("exclude");
    let excludes = builtin(table, "get_ci", vec![matrix, exclude_key])?;
    let after_exclude = builtin(table, "reject_where", vec![product, excludes])?;
    let include_key = table.lit("include");
    let includes = builtin(table, "get_ci", vec![matrix, include_key])?;
    let protected = builtin(table, "keys", vec![axes])?;
    builtin(
        table,
        "extend_where",
        vec![after_exclude, includes, protected],
    )
}
