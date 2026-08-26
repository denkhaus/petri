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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::ids::ExprId;
use crate::runtime::RunContext;

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

/// Per-firing bindings that are neither the token payload nor run-scoped state.
///
/// Scope `env`, the node's identity and generation, the firing's own outcome where
/// there is one, and `item` / `index` inside an expansion clone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StaticCtx {
    vars: BTreeMap<SmolStr, Value>,
}

/// Everything an expression may read.
///
/// One environment for guards, `map`, preconditions and `Expansion.items` alike, so
/// there is a single way for an expression to see upstream state.
///
/// Name resolution runs in this order, and the first three shadow `statics`:
///
/// | Name | Resolves to |
/// |---|---|
/// | `nodes` | [`RunContext::nodes`] — every completed node instance by name |
/// | `kv` | [`RunContext::kv`] — run-scoped key/value state |
/// | `token`, `input` | the token payload |
/// | anything else | [`StaticCtx`] |
pub struct EvalEnv<'a> {
    pub token: &'a Value,
    pub run: &'a RunContext,
    pub statics: &'a StaticCtx,
}

impl<'a> EvalEnv<'a> {
    pub fn new(token: &'a Value, run: &'a RunContext, statics: &'a StaticCtx) -> Self {
        Self {
            token,
            run,
            statics,
        }
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        match name {
            "nodes" => Some(self.run.nodes_value()),
            "kv" => Some(self.run.kv_value()),
            "token" | "input" => Some(self.token.clone()),
            other => self.statics.get(other).cloned(),
        }
    }
}

impl StaticCtx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(mut self, name: &str, value: Value) -> Self {
        self.vars.insert(SmolStr::new(name), value);
        self
    }

    pub fn set(&mut self, name: &str, value: Value) {
        self.vars.insert(SmolStr::new(name), value);
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name)
    }

    pub fn vars(&self) -> &BTreeMap<SmolStr, Value> {
        &self.vars
    }
}

/// One entry in the built-in function table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    pub arity: usize,
    pub summary: &'static str,
}

/// Every function an expression may call.
///
/// This table is not documentation that sits beside the implementation — it **gates**
/// it. [`eval`] looks a call up here before dispatching, so a function that is not in
/// the table is unknown however many match arms exist, and an entry with no arm fails
/// its own conformance test. The two cannot drift.
///
/// # The bar for adding one
///
/// A language that grows a function whenever a test needs one becomes an ad-hoc
/// scripting language by accretion. Every entry must be:
///
/// - **pure** — no IO, no clock, no randomness, no ambient state;
/// - **total** — every input either yields a value or a typed [`EvalError`], never a
///   panic;
/// - **tested** — in `crates/ir/tests/expressions.rs`, including its error cases;
/// - **necessary** — justified by a test or a frontend mapping that genuinely cannot
///   be written without it. `split` met this bar: the outputs-file protocol yields
///   strings, and `for_each` needs an array.
///
/// This is also the surface a frontend expression grammar maps onto, so additions
/// widen a contract rather than adding a convenience.
pub const BUILTINS: &[Builtin] = &[
    // Status predicates. `status` is bound wherever an outcome is in scope.
    Builtin {
        name: "always",
        arity: 0,
        summary: "true, whatever the status",
    },
    Builtin {
        name: "never",
        arity: 0,
        summary: "false, whatever the status",
    },
    Builtin {
        name: "success",
        arity: 0,
        summary: "success-like: Success or PartialSuccess (see Status::is_success_like)",
    },
    Builtin {
        name: "partial_success",
        arity: 0,
        summary: "exactly PartialSuccess",
    },
    Builtin {
        name: "full_success",
        arity: 0,
        summary: "exactly Success, excluding PartialSuccess",
    },
    Builtin {
        name: "failure",
        arity: 0,
        summary: "exactly Failure",
    },
    Builtin {
        name: "skipped",
        arity: 0,
        summary: "exactly Skipped",
    },
    Builtin {
        name: "cancelled",
        arity: 0,
        summary: "exactly Cancelled",
    },
    Builtin {
        name: "timed_out",
        arity: 0,
        summary: "exactly TimedOut",
    },
    // Values.
    Builtin {
        name: "not",
        arity: 1,
        summary: "logical negation, by truthiness",
    },
    Builtin {
        name: "len",
        arity: 1,
        summary: "length of an array, object or string",
    },
    Builtin {
        name: "to_string",
        arity: 1,
        summary: "render a value as a string",
    },
    Builtin {
        name: "default",
        arity: 2,
        summary: "the first value unless it is null, else the second",
    },
    Builtin {
        name: "get",
        arity: 2,
        summary: "index an array by number or an object by key",
    },
    Builtin {
        name: "contains",
        arity: 2,
        summary: "membership in an array, object keys, or a substring",
    },
    Builtin {
        name: "concat",
        arity: 2,
        summary: "join two arrays, strings or objects",
    },
    // Lists. These exist so a collector can reassemble expansion results, and so a
    // step's string output can feed `for_each`, without the language needing lambdas.
    Builtin {
        name: "split",
        arity: 2,
        summary: "split a string on a separator, dropping empty pieces",
    },
    Builtin {
        name: "sort_by_key",
        arity: 2,
        summary: "order an array of objects by one field",
    },
    Builtin {
        name: "pluck",
        arity: 2,
        summary: "take one field from every object in an array",
    },
    // Loose semantics: the JavaScript-family coercion rules most CI expression
    // languages share. A frontend whose format compares loosely lowers its operators
    // onto these instead of `Eq` / `Lt`. Rules are in `crate::loose`.
    Builtin {
        name: "loose_eq",
        arity: 2,
        summary: "loose `==`: differing kinds coerce to numbers; strings compare case-insensitively",
    },
    Builtin {
        name: "loose_lt",
        arity: 2,
        summary: "loose `<`; false whenever a side coerces to NaN",
    },
    Builtin {
        name: "loose_le",
        arity: 2,
        summary: "loose `<=`",
    },
    Builtin {
        name: "loose_gt",
        arity: 2,
        summary: "loose `>`",
    },
    Builtin {
        name: "loose_ge",
        arity: 2,
        summary: "loose `>=`",
    },
    Builtin {
        name: "loose_truthy",
        arity: 1,
        summary: "loose truthiness: empty arrays and objects are truthy",
    },
    Builtin {
        name: "loose_number",
        arity: 1,
        summary: "loose number coercion; NaN becomes null",
    },
    Builtin {
        name: "loose_string",
        arity: 1,
        summary: "loose string coercion: null is empty, containers render as their type name",
    },
    Builtin {
        name: "contains_ci",
        arity: 2,
        summary: "array membership by loose equality, or case-insensitive substring",
    },
    Builtin {
        name: "starts_with",
        arity: 2,
        summary: "case-insensitive prefix test over string coercions",
    },
    Builtin {
        name: "ends_with",
        arity: 2,
        summary: "case-insensitive suffix test over string coercions",
    },
    Builtin {
        name: "format",
        arity: 2,
        summary: "positional `{N}` substitution from an array of arguments; `{{` and `}}` are literal braces",
    },
    Builtin {
        name: "join",
        arity: 2,
        summary: "join an array with a separator, coercing each element to a string",
    },
    Builtin {
        name: "to_json",
        arity: 1,
        summary: "pretty-printed JSON",
    },
    Builtin {
        name: "from_json",
        arity: 1,
        summary: "parse a JSON string; a non-string passes through",
    },
    // Records and lists of records.
    Builtin {
        name: "get_ci",
        arity: 2,
        summary: "case-insensitive property lookup",
    },
    Builtin {
        name: "values",
        arity: 1,
        summary: "an object's values, or an array itself",
    },
    Builtin {
        name: "pluck_present",
        arity: 2,
        summary: "one field from every record that has it; records without it are dropped",
    },
    Builtin {
        name: "keys",
        arity: 1,
        summary: "an object's keys, in order",
    },
    Builtin {
        name: "omit",
        arity: 2,
        summary: "an object without the named keys",
    },
    Builtin {
        name: "cartesian",
        arity: 1,
        summary: "every combination of one value per key of an object of arrays",
    },
    Builtin {
        name: "reject_where",
        arity: 2,
        summary: "drop every record matching any of the partial records",
    },
    Builtin {
        name: "extend_where",
        arity: 3,
        summary: "merge partial records into compatible records, protecting the named keys; append the rest",
    },
];

/// Look a function up in the table.
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum EvalError {
    #[error("expression {0} is not in the table")]
    UnknownExpr(ExprId),
    #[error("`{0}` is not bound in this context")]
    UnboundVar(SmolStr),
    #[error("unknown function `{0}`")]
    UnknownFunction(SmolStr),
    #[error("`{name}` takes {expected} argument(s), got {got}")]
    Arity {
        name: SmolStr,
        expected: usize,
        got: usize,
    },
    #[error("{op} needs {expected}, got {got}")]
    Type {
        op: SmolStr,
        expected: SmolStr,
        got: SmolStr,
    },
    #[error("division by zero")]
    DivByZero,
    #[error("expression nesting is too deep")]
    RecursionLimit,
}

const MAX_DEPTH: u32 = 256;

/// Evaluate `id` against `env`.
pub fn eval(table: &ExprTable, id: ExprId, env: &EvalEnv<'_>) -> Result<Value, EvalError> {
    eval_at(table, id, env, 0)
}

/// Evaluate `id` and read the result as a boolean, using [`truthy`].
pub fn eval_bool(table: &ExprTable, id: ExprId, env: &EvalEnv<'_>) -> Result<bool, EvalError> {
    Ok(truthy(&eval(table, id, env)?))
}

/// JSON truthiness: `null`, `false`, `0`, `""` and empty containers are false.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn eval_at(
    table: &ExprTable,
    id: ExprId,
    env: &EvalEnv<'_>,
    depth: u32,
) -> Result<Value, EvalError> {
    if depth > MAX_DEPTH {
        return Err(EvalError::RecursionLimit);
    }
    let expr = table.get(id).ok_or(EvalError::UnknownExpr(id))?;
    let d = depth + 1;
    match expr {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Var(name) => env
            .lookup(name)
            .ok_or_else(|| EvalError::UnboundVar(name.clone())),
        Expr::Field(base, name) => {
            let base = eval_at(table, *base, env, d)?;
            Ok(base.get(name.as_str()).cloned().unwrap_or(Value::Null))
        }
        Expr::Index(base, idx) => {
            let base = eval_at(table, *base, env, d)?;
            let idx = eval_at(table, *idx, env, d)?;
            Ok(index_into(&base, &idx))
        }
        Expr::Unary(op, arg) => {
            let v = eval_at(table, *arg, env, d)?;
            match op {
                UnOp::Not => Ok(Value::Bool(!truthy(&v))),
                UnOp::Neg => as_f64("-", &v).map(|n| num(-n)),
            }
        }
        Expr::Binary(op, lhs, rhs) => eval_binary(table, *op, *lhs, *rhs, env, d),
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => {
            if truthy(&eval_at(table, *cond, env, d)?) {
                eval_at(table, *then, env, d)
            } else {
                eval_at(table, *otherwise, env, d)
            }
        }
        Expr::Array(items) => items
            .iter()
            .map(|i| eval_at(table, *i, env, d))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Expr::Object(fields) => {
            let mut map = serde_json::Map::with_capacity(fields.len());
            for (k, v) in fields {
                map.insert(k.to_string(), eval_at(table, *v, env, d)?);
            }
            Ok(Value::Object(map))
        }
        Expr::Call(name, args) => eval_call(table, name, args, env, d),
    }
}

fn eval_binary(
    table: &ExprTable,
    op: BinOp,
    lhs: ExprId,
    rhs: ExprId,
    env: &EvalEnv<'_>,
    depth: u32,
) -> Result<Value, EvalError> {
    // `and`/`or` short-circuit, so the right side is only evaluated when needed.
    match op {
        BinOp::And => {
            let l = eval_at(table, lhs, env, depth)?;
            return if truthy(&l) {
                Ok(Value::Bool(truthy(&eval_at(table, rhs, env, depth)?)))
            } else {
                Ok(Value::Bool(false))
            };
        }
        BinOp::Or => {
            let l = eval_at(table, lhs, env, depth)?;
            return if truthy(&l) {
                Ok(Value::Bool(true))
            } else {
                Ok(Value::Bool(truthy(&eval_at(table, rhs, env, depth)?)))
            };
        }
        _ => {}
    }

    let l = eval_at(table, lhs, env, depth)?;
    let r = eval_at(table, rhs, env, depth)?;
    match op {
        BinOp::Eq => Ok(Value::Bool(l == r)),
        BinOp::Ne => Ok(Value::Bool(l != r)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let name = op_name(op);
            let ord = compare(name, &l, &r)?;
            Ok(Value::Bool(match op {
                BinOp::Lt => ord.is_lt(),
                BinOp::Le => ord.is_le(),
                BinOp::Gt => ord.is_gt(),
                _ => ord.is_ge(),
            }))
        }
        BinOp::Add => {
            // `+` concatenates when either side is a string, matching frontends.
            if l.is_string() || r.is_string() {
                Ok(Value::String(format!(
                    "{}{}",
                    to_display(&l),
                    to_display(&r)
                )))
            } else {
                Ok(num(as_f64("+", &l)? + as_f64("+", &r)?))
            }
        }
        BinOp::Sub => Ok(num(as_f64("-", &l)? - as_f64("-", &r)?)),
        BinOp::Mul => Ok(num(as_f64("*", &l)? * as_f64("*", &r)?)),
        BinOp::Div => {
            let d = as_f64("/", &r)?;
            if d == 0.0 {
                return Err(EvalError::DivByZero);
            }
            Ok(num(as_f64("/", &l)? / d))
        }
        BinOp::Rem => {
            let d = as_f64("%", &r)?;
            if d == 0.0 {
                return Err(EvalError::DivByZero);
            }
            Ok(num(as_f64("%", &l)? % d))
        }
        BinOp::Concat => concat(&l, &r),
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

fn eval_call(
    table: &ExprTable,
    name: &SmolStr,
    args: &[ExprId],
    env: &EvalEnv<'_>,
    depth: u32,
) -> Result<Value, EvalError> {
    // The table gates dispatch: an unknown name never reaches a match arm, and arity
    // is checked once, here, rather than in nineteen places.
    let spec = builtin(name).ok_or_else(|| EvalError::UnknownFunction(name.clone()))?;
    if args.len() != spec.arity {
        return Err(EvalError::Arity {
            name: name.clone(),
            expected: spec.arity,
            got: args.len(),
        });
    }
    let arg = |i: usize| eval_at(table, args[i], env, depth);
    // `status` is bound by the core wherever an outcome is in scope.
    let status_is = |want: &str| -> Result<Value, EvalError> {
        let s = env.lookup("status").unwrap_or(Value::Null);
        Ok(Value::Bool(s.as_str() == Some(want)))
    };

    match name.as_str() {
        // Status predicates over the bound `status`.
        "always" => Ok(Value::Bool(true)),
        "never" => Ok(Value::Bool(false)),
        // `success()` is success-like, per Status::is_success_like: it is the default
        // success guard, and it must not open-code the classification.
        "success" => {
            let tag = env.lookup("status").unwrap_or(Value::Null);
            let tag = tag.as_str().unwrap_or_default();
            Ok(Value::Bool(tag == "success" || tag == "partial_success"))
        }
        // Exactly `PartialSuccess`, for a guard that needs to tell the two apart.
        "partial_success" => status_is("partial_success"),
        // Strictly `Success`, excluding `PartialSuccess`.
        "full_success" => status_is("success"),
        "failure" => status_is("failure"),
        "skipped" => status_is("skipped"),
        "cancelled" => status_is("cancelled"),
        "timed_out" => status_is("timed_out"),
        "len" => {
            let v = arg(0)?;
            let n = match &v {
                Value::Array(a) => a.len(),
                Value::Object(o) => o.len(),
                Value::String(s) => s.chars().count(),
                Value::Null => 0,
                other => return Err(type_err("len", "array, object or string", other)),
            };
            Ok(num(n as f64))
        }
        "concat" => concat(&arg(0)?, &arg(1)?),
        "contains" => {
            let (hay, needle) = (arg(0)?, arg(1)?);
            Ok(Value::Bool(match &hay {
                Value::Array(a) => a.contains(&needle),
                Value::Object(o) => needle.as_str().is_some_and(|k| o.contains_key(k)),
                Value::String(s) => needle.as_str().is_some_and(|n| s.contains(n)),
                other => return Err(type_err("contains", "array, object or string", other)),
            }))
        }
        "get" => Ok(index_into(&arg(0)?, &arg(1)?)),
        "default" => {
            let v = arg(0)?;
            Ok(if v.is_null() { arg(1)? } else { v })
        }
        "sort_by_key" => {
            // Order an array of objects by one field. Lets a collector put clone
            // results back in `index` order without needing lambdas.
            let (array, key) = (arg(0)?, arg(1)?);
            let Value::Array(mut items) = array else {
                return Err(type_err("sort_by_key", "an array", &array));
            };
            let key = key
                .as_str()
                .ok_or_else(|| type_err("sort_by_key", "a string key", &key))?
                .to_string();
            items.sort_by(|a, b| {
                let (a, b) = (a.get(&key), b.get(&key));
                match (a, b) {
                    (Some(Value::Number(x)), Some(Value::Number(y))) => x
                        .as_f64()
                        .partial_cmp(&y.as_f64())
                        .unwrap_or(std::cmp::Ordering::Equal),
                    (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                }
            });
            Ok(Value::Array(items))
        }
        "split" => {
            // The outputs-file protocol yields strings, so turning one into a list
            // is what a frontend needs to feed `for_each`. Empty trailing segments
            // are dropped, which is what a trailing newline means in practice.
            let (text, separator) = (arg(0)?, arg(1)?);
            let text = text
                .as_str()
                .ok_or_else(|| type_err("split", "a string", &text))?;
            let separator = separator
                .as_str()
                .ok_or_else(|| type_err("split", "a string separator", &separator))?;
            if separator.is_empty() {
                return Err(type_err("split", "a non-empty separator", &Value::Null));
            }
            let parts: Vec<Value> = text
                .split(separator)
                .filter(|piece| !piece.is_empty())
                .map(|piece| Value::String(piece.to_string()))
                .collect();
            Ok(Value::Array(parts))
        }
        "pluck" => {
            let (array, key) = (arg(0)?, arg(1)?);
            let Value::Array(items) = array else {
                return Err(type_err("pluck", "an array", &array));
            };
            let key = key
                .as_str()
                .ok_or_else(|| type_err("pluck", "a string key", &key))?
                .to_string();
            Ok(Value::Array(
                items
                    .iter()
                    .map(|i| i.get(&key).cloned().unwrap_or(Value::Null))
                    .collect(),
            ))
        }
        // ── Loose semantics ───────────────────────────────────────────────
        "loose_eq" => Ok(Value::Bool(crate::loose::equal(&arg(0)?, &arg(1)?))),
        "loose_lt" | "loose_le" | "loose_gt" | "loose_ge" => {
            let ord = crate::loose::compare(&arg(0)?, &arg(1)?);
            Ok(Value::Bool(match (name.as_str(), ord) {
                (_, None) => false,
                ("loose_lt", Some(o)) => o.is_lt(),
                ("loose_le", Some(o)) => o.is_le(),
                ("loose_gt", Some(o)) => o.is_gt(),
                (_, Some(o)) => o.is_ge(),
            }))
        }
        "loose_truthy" => Ok(Value::Bool(crate::loose::truthy(&arg(0)?))),
        "loose_number" => Ok(num(crate::loose::to_number(&arg(0)?))),
        "loose_string" => Ok(Value::String(crate::loose::to_string(&arg(0)?))),
        "contains_ci" => {
            let (search, item) = (arg(0)?, arg(1)?);
            Ok(Value::Bool(match &search {
                Value::Array(items) => items.iter().any(|i| crate::loose::equal(i, &item)),
                Value::Object(_) => false,
                primitive => match &item {
                    Value::Array(_) | Value::Object(_) => false,
                    _ => crate::loose::to_string(primitive)
                        .to_lowercase()
                        .contains(&crate::loose::to_string(&item).to_lowercase()),
                },
            }))
        }
        "starts_with" | "ends_with" => {
            let (text, probe) = (arg(0)?, arg(1)?);
            if matches!(text, Value::Array(_) | Value::Object(_))
                || matches!(probe, Value::Array(_) | Value::Object(_))
            {
                return Ok(Value::Bool(false));
            }
            let text = crate::loose::to_string(&text).to_lowercase();
            let probe = crate::loose::to_string(&probe).to_lowercase();
            Ok(Value::Bool(if name == "starts_with" {
                text.starts_with(&probe)
            } else {
                text.ends_with(&probe)
            }))
        }
        "format" => {
            let (template, args_value) = (arg(0)?, arg(1)?);
            let template = crate::loose::to_string(&template);
            let values: Vec<Value> = match args_value {
                Value::Array(items) => items,
                other => vec![other],
            };
            positional_format(&template, &values)
        }
        "join" => {
            let (items, separator) = (arg(0)?, arg(1)?);
            let separator = if separator.is_null() {
                ",".to_string()
            } else {
                crate::loose::to_string(&separator)
            };
            Ok(Value::String(match &items {
                Value::Array(items) => items
                    .iter()
                    .map(crate::loose::to_string)
                    .collect::<Vec<_>>()
                    .join(&separator),
                other => crate::loose::to_string(other),
            }))
        }
        "to_json" => Ok(Value::String(
            serde_json::to_string_pretty(&arg(0)?).unwrap_or_default(),
        )),
        "from_json" => {
            let v = arg(0)?;
            match &v {
                Value::String(s) => serde_json::from_str::<Value>(s).map_err(|e| EvalError::Type {
                    op: SmolStr::new("from_json"),
                    expected: SmolStr::new("valid JSON"),
                    got: SmolStr::new(format!("invalid JSON ({e})")),
                }),
                _ => Ok(v),
            }
        }
        "get_ci" => {
            let (object, key) = (arg(0)?, arg(1)?);
            let Some(key) = key.as_str() else {
                return Ok(Value::Null);
            };
            Ok(crate::loose::get_ci(&object, key)
                .cloned()
                .unwrap_or(Value::Null))
        }
        "values" => Ok(match arg(0)? {
            Value::Object(map) => Value::Array(map.into_iter().map(|(_, v)| v).collect()),
            array @ Value::Array(_) => array,
            _ => Value::Null,
        }),
        "pluck_present" => {
            let (items, key) = (arg(0)?, arg(1)?);
            let (Value::Array(items), Some(key)) = (items, key.as_str()) else {
                return Ok(Value::Null);
            };
            Ok(Value::Array(
                items
                    .iter()
                    .filter_map(|item| crate::loose::get_ci(item, key).cloned())
                    .collect(),
            ))
        }
        "keys" => Ok(Value::Array(crate::combine::keys(&arg(0)?))),
        "omit" => Ok(crate::combine::omit(&arg(0)?, &arg(1)?)),
        "cartesian" => Ok(Value::Array(crate::combine::cartesian(&arg(0)?))),
        "reject_where" => Ok(Value::Array(crate::combine::reject_where(
            &arg(0)?,
            &arg(1)?,
        ))),
        "extend_where" => Ok(Value::Array(crate::combine::extend_where(
            &arg(0)?,
            &arg(1)?,
            &arg(2)?,
        ))),
        "to_string" => Ok(Value::String(to_display(&arg(0)?))),
        "not" => Ok(Value::Bool(!truthy(&arg(0)?))),
        // Unreachable: the table gated this call, so every entry has an arm above.
        // A new table entry with no arm lands here and fails its conformance test.
        _ => Err(EvalError::UnknownFunction(name.clone())),
    }
}

/// Positional formatting: `{N}` substitutes argument N, `{{` and `}}` are literal
/// braces. An index with no argument is an error rather than an empty string.
fn positional_format(template: &str, args: &[Value]) -> Result<Value, EvalError> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                let mut digits = String::new();
                while let Some(d) = chars.peek().copied() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if digits.is_empty() || chars.next() != Some('}') {
                    return Err(EvalError::Type {
                        op: SmolStr::new("format"),
                        expected: SmolStr::new("`{N}` placeholders"),
                        got: SmolStr::new("a malformed placeholder"),
                    });
                }
                let index: usize = digits.parse().unwrap_or(usize::MAX);
                match args.get(index) {
                    Some(v) => out.push_str(&crate::loose::to_string(v)),
                    None => {
                        return Err(EvalError::Type {
                            op: SmolStr::new("format"),
                            expected: SmolStr::new(format!("at least {} argument(s)", index + 1)),
                            got: SmolStr::new(format!("{}", args.len())),
                        });
                    }
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            other => out.push(other),
        }
    }
    Ok(Value::String(out))
}

fn index_into(base: &Value, idx: &Value) -> Value {
    match (base, idx) {
        (Value::Array(a), Value::Number(n)) => n
            .as_u64()
            .and_then(|i| a.get(i as usize))
            .cloned()
            .unwrap_or(Value::Null),
        (Value::Object(o), Value::String(k)) => o.get(k.as_str()).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn concat(l: &Value, r: &Value) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Array(a), Value::Array(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            Ok(Value::Array(out))
        }
        (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
        (Value::Object(a), Value::Object(b)) => {
            let mut out = a.clone();
            for (k, v) in b {
                out.insert(k.clone(), v.clone());
            }
            Ok(Value::Object(out))
        }
        (a, _) => Err(type_err("++", "two arrays, strings or objects", a)),
    }
}

fn compare(op: &str, l: &Value, r: &Value) -> Result<std::cmp::Ordering, EvalError> {
    match (l, r) {
        (Value::Number(_), Value::Number(_)) => {
            let (a, b) = (as_f64(op, l)?, as_f64(op, r)?);
            a.partial_cmp(&b)
                .ok_or_else(|| type_err(op, "comparable numbers", l))
        }
        (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
        (a, _) => Err(type_err(op, "two numbers or two strings", a)),
    }
}

fn as_f64(op: &str, v: &Value) -> Result<f64, EvalError> {
    v.as_f64().ok_or_else(|| type_err(op, "a number", v))
}

fn num(f: f64) -> Value {
    // Integral results stay integers so `idx + 1` indexes arrays cleanly.
    if f.fract() == 0.0 && f.is_finite() && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

fn to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn type_err(op: &str, expected: &str, got: &Value) -> EvalError {
    EvalError::Type {
        op: SmolStr::new(op),
        expected: SmolStr::new(expected),
        got: SmolStr::new(type_name(got)),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Concat => "++",
    }
}
