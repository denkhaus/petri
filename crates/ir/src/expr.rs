//! Expressions: a small, total, side-effect-free language evaluated in the pure core.
//!
//! Expressions are stored flat in an [`ExprTable`] and referenced by [`ExprId`], so a
//! [`Graph`](crate::Graph) stays a plain tree of `Copy` ids with no interior pointers.
//! Evaluation has no IO, no clocks and no randomness: the same [`Context`] always
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

/// One expression node. Sub-expressions are referenced by [`ExprId`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// A literal JSON value.
    Lit(Value),
    /// A binding looked up in the [`Context`] (`outcome`, `output`, `item`, `env`, ...).
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

/// Bindings an expression is evaluated against.
///
/// The core builds one of these per evaluation site: routing guards and `map`
/// expressions see the completing node's outcome, preconditions see the node's own
/// context, and expansion clones additionally see `item` and `index`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Context {
    vars: BTreeMap<SmolStr, Value>,
}

impl Context {
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

/// Evaluate `id` against `ctx`.
pub fn eval(table: &ExprTable, id: ExprId, ctx: &Context) -> Result<Value, EvalError> {
    eval_at(table, id, ctx, 0)
}

/// Evaluate `id` and read the result as a boolean, using [`truthy`].
pub fn eval_bool(table: &ExprTable, id: ExprId, ctx: &Context) -> Result<bool, EvalError> {
    Ok(truthy(&eval(table, id, ctx)?))
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

fn eval_at(table: &ExprTable, id: ExprId, ctx: &Context, depth: u32) -> Result<Value, EvalError> {
    if depth > MAX_DEPTH {
        return Err(EvalError::RecursionLimit);
    }
    let expr = table.get(id).ok_or(EvalError::UnknownExpr(id))?;
    let d = depth + 1;
    match expr {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Var(name) => ctx
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::UnboundVar(name.clone())),
        Expr::Field(base, name) => {
            let base = eval_at(table, *base, ctx, d)?;
            Ok(base.get(name.as_str()).cloned().unwrap_or(Value::Null))
        }
        Expr::Index(base, idx) => {
            let base = eval_at(table, *base, ctx, d)?;
            let idx = eval_at(table, *idx, ctx, d)?;
            Ok(index_into(&base, &idx))
        }
        Expr::Unary(op, arg) => {
            let v = eval_at(table, *arg, ctx, d)?;
            match op {
                UnOp::Not => Ok(Value::Bool(!truthy(&v))),
                UnOp::Neg => as_f64("-", &v).map(|n| num(-n)),
            }
        }
        Expr::Binary(op, lhs, rhs) => eval_binary(table, *op, *lhs, *rhs, ctx, d),
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => {
            if truthy(&eval_at(table, *cond, ctx, d)?) {
                eval_at(table, *then, ctx, d)
            } else {
                eval_at(table, *otherwise, ctx, d)
            }
        }
        Expr::Array(items) => items
            .iter()
            .map(|i| eval_at(table, *i, ctx, d))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Expr::Object(fields) => {
            let mut map = serde_json::Map::with_capacity(fields.len());
            for (k, v) in fields {
                map.insert(k.to_string(), eval_at(table, *v, ctx, d)?);
            }
            Ok(Value::Object(map))
        }
        Expr::Call(name, args) => eval_call(table, name, args, ctx, d),
    }
}

fn eval_binary(
    table: &ExprTable,
    op: BinOp,
    lhs: ExprId,
    rhs: ExprId,
    ctx: &Context,
    depth: u32,
) -> Result<Value, EvalError> {
    // `and`/`or` short-circuit, so the right side is only evaluated when needed.
    match op {
        BinOp::And => {
            let l = eval_at(table, lhs, ctx, depth)?;
            return if truthy(&l) {
                Ok(Value::Bool(truthy(&eval_at(table, rhs, ctx, depth)?)))
            } else {
                Ok(Value::Bool(false))
            };
        }
        BinOp::Or => {
            let l = eval_at(table, lhs, ctx, depth)?;
            return if truthy(&l) {
                Ok(Value::Bool(true))
            } else {
                Ok(Value::Bool(truthy(&eval_at(table, rhs, ctx, depth)?)))
            };
        }
        _ => {}
    }

    let l = eval_at(table, lhs, ctx, depth)?;
    let r = eval_at(table, rhs, ctx, depth)?;
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
    ctx: &Context,
    depth: u32,
) -> Result<Value, EvalError> {
    let arity = |expected: usize| -> Result<(), EvalError> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(EvalError::Arity {
                name: name.clone(),
                expected,
                got: args.len(),
            })
        }
    };
    let arg = |i: usize| eval_at(table, args[i], ctx, depth);
    // `status` is bound by the core wherever an outcome is in scope.
    let status_is = |want: &str| -> Result<Value, EvalError> {
        let s = ctx.get("status").cloned().unwrap_or(Value::Null);
        Ok(Value::Bool(s.as_str() == Some(want)))
    };

    match name.as_str() {
        // GHA-style status functions.
        "always" => {
            arity(0)?;
            Ok(Value::Bool(true))
        }
        "never" => {
            arity(0)?;
            Ok(Value::Bool(false))
        }
        "success" => {
            arity(0)?;
            status_is("success")
        }
        "failure" => {
            arity(0)?;
            status_is("failure")
        }
        "skipped" => {
            arity(0)?;
            status_is("skipped")
        }
        "cancelled" => {
            arity(0)?;
            status_is("cancelled")
        }
        "timed_out" => {
            arity(0)?;
            status_is("timed_out")
        }
        "len" => {
            arity(1)?;
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
        "concat" => {
            arity(2)?;
            concat(&arg(0)?, &arg(1)?)
        }
        "contains" => {
            arity(2)?;
            let (hay, needle) = (arg(0)?, arg(1)?);
            Ok(Value::Bool(match &hay {
                Value::Array(a) => a.contains(&needle),
                Value::Object(o) => needle.as_str().is_some_and(|k| o.contains_key(k)),
                Value::String(s) => needle.as_str().is_some_and(|n| s.contains(n)),
                other => return Err(type_err("contains", "array, object or string", other)),
            }))
        }
        "get" => {
            arity(2)?;
            Ok(index_into(&arg(0)?, &arg(1)?))
        }
        "default" => {
            arity(2)?;
            let v = arg(0)?;
            Ok(if v.is_null() { arg(1)? } else { v })
        }
        "sort_by_key" => {
            // Order an array of objects by one field. Lets a collector put clone
            // results back in `index` order without needing lambdas.
            arity(2)?;
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
        "pluck" => {
            arity(2)?;
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
        "to_string" => {
            arity(1)?;
            Ok(Value::String(to_display(&arg(0)?)))
        }
        "not" => {
            arity(1)?;
            Ok(Value::Bool(!truthy(&arg(0)?)))
        }
        _ => Err(EvalError::UnknownFunction(name.clone())),
    }
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
