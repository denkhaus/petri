//! Evaluation: the environment an expression reads, and the walker that reduces it
//! to a [`Value`]. Function calls dispatch through [`super::builtins`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use super::builtins::eval_call;
use super::{BinOp, Expr, ExprTable, UnOp};
use crate::flow::RunContext;
use crate::ids::ExprId;

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

    pub(super) fn lookup(&self, name: &str) -> Option<Value> {
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

pub(super) fn eval_at(
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
            // `nodes.x` and `kv.x` read one entry from the run context directly:
            // materializing the whole map first would clone every node's output to
            // read one field. `nodes` and `kv` cannot be shadowed, so this is
            // observationally the same as the general path.
            if let Some(Expr::Var(var)) = table.get(*base) {
                match var.as_str() {
                    "nodes" => {
                        return Ok(env
                            .run
                            .node(name)
                            .map(|record| record.to_value())
                            .unwrap_or(Value::Null));
                    }
                    "kv" => return Ok(env.run.get(name).cloned().unwrap_or(Value::Null)),
                    _ => {}
                }
            }
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

pub(super) fn index_into(base: &Value, idx: &Value) -> Value {
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

pub(super) fn concat(l: &Value, r: &Value) -> Result<Value, EvalError> {
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

pub(super) fn num(f: f64) -> Value {
    // Integral results stay integers so `idx + 1` indexes arrays cleanly.
    if f.fract() == 0.0 && f.is_finite() && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

pub(super) fn to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(super) fn type_err(op: &str, expected: &str, got: &Value) -> EvalError {
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
