//! The step-level condition gate: its wire format, its evaluator, and the
//! lowering from a parsed condition onto it.
//!
//! A step-level `if:` (and an action's `pre-if` / `post-if`) does not become an
//! engine precondition. It lowers into the step's config as a small expression
//! tree — the **gate** — and the step kind evaluates it at spawn, before session
//! files are created and before any process starts. One mechanism evaluates every
//! step-level condition; nothing inspects condition text to decide admission.
//!
//! Leaves of the gate:
//!
//! - A maximal subtree that needs nothing from the step — no `hashFiles`, no
//!   `env.*` — lowers to one engine expression and rides as `{"lit": {"$expr": id}}`.
//!   The engine resolves it to a literal when the step fires, as it does for any
//!   config value, so the step sees `{"lit": value}`.
//! - `env.NAME` becomes `{"$env": "NAME", "or": <gate>}`: the step resolves the
//!   name from its own environment — its `env:` config and the job's accumulated
//!   `GITHUB_ENV` — so it sees what earlier steps appended. The `or` fallback is
//!   the engine's view of the same reference (the scope env), for names declared
//!   in the workflow rather than written at run time.
//! - `hashFiles(...)` with literal patterns is a `{"lit": "<sentinel>"}` string
//!   leaf; the step resolves the sentinel against the workspace before comparing.
//! - `secrets.*` stays rejected in conditions: gates carry no secret leaves.
//!
//! Interior nodes are the GitHub operators — `==`, `!=`, `<`, `<=`, `>`, `>=`,
//! `&&`, `||`, `!` — with GitHub's semantics: `&&`/`||` return operand values, and
//! the caller applies GitHub truthiness to the root. Comparison and coercion come
//! from the engine's own loose primitives ([`ir::expr::builtins::loose`]), so the
//! two evaluators cannot drift.
//!
//! Every literal is wrapped as `{"lit": …}` — unlike the bare values a first
//! sketch might use — because a resolved engine leaf can be *any* JSON value, and
//! an unwrapped object could not be told apart from an operator node.

use frontend::diag::{Diagnostics, Span};
use frontend::expr::lower::literal_value;
use frontend::expr::{Expr, UnaryOp};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{ExprTable, Value};
use serde_json::json;

use crate::exprs::{Site, hashfiles_sentinel, literal_hashfiles_patterns, lower_expr};

/// Config key for an interior node's operator.
pub const OP_KEY: &str = "op";
/// Config key for an interior node's operands.
pub const ARGS_KEY: &str = "args";
/// Config key wrapping a literal leaf (or an `{"$expr": id}` the engine resolves
/// into one).
pub const LIT_KEY: &str = "lit";
/// Config key for an env leaf: the variable name the step resolves.
pub const ENV_KEY: &str = "$env";
/// Config key for an env leaf's fallback, used when the step's environment does
/// not bind the name.
pub const ENV_OR_KEY: &str = "or";

/// A gate operator: exactly the operators GitHub's condition grammar has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
}

impl GateOp {
    pub fn symbol(self) -> &'static str {
        match self {
            GateOp::Eq => "==",
            GateOp::Ne => "!=",
            GateOp::Lt => "<",
            GateOp::Le => "<=",
            GateOp::Gt => ">",
            GateOp::Ge => ">=",
            GateOp::And => "&&",
            GateOp::Or => "||",
            GateOp::Not => "!",
        }
    }

    pub fn parse(symbol: &str) -> Option<Self> {
        Some(match symbol {
            "==" => GateOp::Eq,
            "!=" => GateOp::Ne,
            "<" => GateOp::Lt,
            "<=" => GateOp::Le,
            ">" => GateOp::Gt,
            ">=" => GateOp::Ge,
            "&&" => GateOp::And,
            "||" => GateOp::Or,
            "!" => GateOp::Not,
            _ => return None,
        })
    }
}

/// One node of a gate.
#[derive(Clone, Debug, PartialEq)]
pub enum Gate {
    /// A literal value. In the graph this may be `{"$expr": id}`, which the engine
    /// resolves before the step sees it; a string may carry a `hashFiles` sentinel
    /// the step resolves before comparing.
    Lit(Value),
    /// An environment variable, resolved by the step; `or` when the step's
    /// environment does not bind it.
    Env { name: String, or: Option<Box<Gate>> },
    /// A GitHub operator over sub-gates.
    Op { op: GateOp, args: Vec<Gate> },
}

impl Gate {
    /// An engine-expression leaf: `{"lit": {"$expr": id}}`.
    pub fn expr(id: ir::ExprId) -> Gate {
        Gate::Lit(json!({ EXPR_PLACEHOLDER_KEY: id.raw() }))
    }

    /// The gate as config JSON.
    pub fn to_value(&self) -> Value {
        match self {
            Gate::Lit(v) => json!({ LIT_KEY: v }),
            Gate::Env { name, or } => match or {
                Some(or) => json!({ ENV_KEY: name, ENV_OR_KEY: or.to_value() }),
                None => json!({ ENV_KEY: name }),
            },
            Gate::Op { op, args } => json!({
                OP_KEY: op.symbol(),
                ARGS_KEY: args.iter().map(Gate::to_value).collect::<Vec<_>>(),
            }),
        }
    }

    /// Read a gate back from config JSON. The format is strict: every node is an
    /// `op`, an `$env`, or a `lit` object, so a resolved engine value can never be
    /// mistaken for structure.
    pub fn from_value(value: &Value) -> Result<Gate, String> {
        let Some(map) = value.as_object() else {
            return Err(format!("a gate node must be an object, got {value}"));
        };
        if let Some(inner) = map.get(LIT_KEY) {
            return Ok(Gate::Lit(inner.clone()));
        }
        if let Some(name) = map.get(ENV_KEY) {
            let Some(name) = name.as_str() else {
                return Err("`$env` must name a variable".to_string());
            };
            let or = match map.get(ENV_OR_KEY) {
                Some(or) => Some(Box::new(Gate::from_value(or)?)),
                None => None,
            };
            return Ok(Gate::Env {
                name: name.to_string(),
                or,
            });
        }
        if let (Some(op), Some(args)) = (map.get(OP_KEY), map.get(ARGS_KEY)) {
            let Some(op) = op.as_str().and_then(GateOp::parse) else {
                return Err(format!("`{op}` is not a gate operator"));
            };
            let Some(args) = args.as_array() else {
                return Err("`args` must be an array".to_string());
            };
            let args = args
                .iter()
                .map(Gate::from_value)
                .collect::<Result<Vec<_>, _>>()?;
            // `!` takes one operand, comparisons exactly two, `&&`/`||` two or more.
            let ok = match op {
                GateOp::Not => args.len() == 1,
                GateOp::And | GateOp::Or => args.len() >= 2,
                _ => args.len() == 2,
            };
            if !ok {
                return Err(format!("`{}` has {} operand(s)", op.symbol(), args.len()));
            }
            return Ok(Gate::Op { op, args });
        }
        Err("a gate node must be `op`/`args`, `$env`, or `lit`".to_string())
    }

    /// Every string literal in the gate, for sentinel resolution.
    pub fn texts(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_texts(&mut out);
        out
    }

    fn collect_texts<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Gate::Lit(Value::String(s)) => out.push(s),
            Gate::Lit(_) => {}
            Gate::Env { or, .. } => {
                if let Some(or) = or {
                    or.collect_texts(out);
                }
            }
            Gate::Op { args, .. } => {
                for arg in args {
                    arg.collect_texts(out);
                }
            }
        }
    }

    /// Rewrite every string literal; `None` keeps a string unchanged.
    pub fn map_texts(&mut self, map: &mut dyn FnMut(&str) -> Option<String>) {
        match self {
            Gate::Lit(Value::String(s)) => {
                if let Some(new) = map(s) {
                    *s = new;
                }
            }
            Gate::Lit(_) => {}
            Gate::Env { or, .. } => {
                if let Some(or) = or {
                    or.map_texts(map);
                }
            }
            Gate::Op { args, .. } => {
                for arg in args {
                    arg.map_texts(map);
                }
            }
        }
    }

    /// Whether any `$env` leaf exists, so a caller can skip loading the
    /// environment when none does.
    pub fn reads_env(&self) -> bool {
        match self {
            Gate::Lit(_) => false,
            Gate::Env { .. } => true,
            Gate::Op { args, .. } => args.iter().any(Gate::reads_env),
        }
    }
}

/// Evaluate a gate to its value, GitHub-style: `&&` and `||` return operand
/// values, comparisons use the loose rules, `!` negates loose truthiness. The
/// caller applies [`loose::truthy`] to the result — GitHub truthiness at the root.
///
/// `env` resolves an env leaf: `Ok(None)` means the name is unbound and the
/// leaf's `or` (or null) stands in.
pub fn eval<E>(
    gate: &Gate,
    env: &mut dyn FnMut(&str) -> Result<Option<Value>, E>,
) -> Result<Value, E> {
    use ir::expr::builtins::loose;
    Ok(match gate {
        Gate::Lit(v) => v.clone(),
        Gate::Env { name, or } => match env(name)? {
            Some(v) => v,
            None => match or {
                Some(or) => eval(or, env)?,
                None => Value::Null,
            },
        },
        Gate::Op { op, args } => match op {
            GateOp::And => {
                let mut acc = eval(&args[0], env)?;
                for next in &args[1..] {
                    if !loose::truthy(&acc) {
                        break;
                    }
                    acc = eval(next, env)?;
                }
                acc
            }
            GateOp::Or => {
                let mut acc = eval(&args[0], env)?;
                for next in &args[1..] {
                    if loose::truthy(&acc) {
                        break;
                    }
                    acc = eval(next, env)?;
                }
                acc
            }
            GateOp::Not => Value::Bool(!loose::truthy(&eval(&args[0], env)?)),
            GateOp::Eq | GateOp::Ne => {
                let (l, r) = (eval(&args[0], env)?, eval(&args[1], env)?);
                let eq = loose::equal(&l, &r);
                Value::Bool(if *op == GateOp::Eq { eq } else { !eq })
            }
            GateOp::Lt | GateOp::Le | GateOp::Gt | GateOp::Ge => {
                let (l, r) = (eval(&args[0], env)?, eval(&args[1], env)?);
                Value::Bool(match loose::compare(&l, &r) {
                    None => false,
                    Some(ord) => match op {
                        GateOp::Lt => ord.is_lt(),
                        GateOp::Le => ord.is_le(),
                        GateOp::Gt => ord.is_gt(),
                        _ => ord.is_ge(),
                    },
                })
            }
        },
    })
}

// ── Lowering a condition onto the gate ────────────────────────────────────

/// Whether the subtree holds something only the step can resolve: an `env.NAME`
/// reference, a `hashFiles(...)` call, `github.workspace`, `runner.temp`, or
/// `runner.tool_cache`.
pub fn needs_lazy(expr: &Expr) -> bool {
    if env_leaf(expr).is_some()
        || workspace_leaf(expr)
        || runner_temp_leaf(expr)
        || runner_tool_cache_leaf(expr)
    {
        return true;
    }
    match expr {
        Expr::Literal(_) | Expr::Ident(_) => false,
        Expr::Property(base, _) | Expr::Wildcard(base) | Expr::Group(base) => needs_lazy(base),
        Expr::Index(base, key) => needs_lazy(base) || needs_lazy(key),
        Expr::Unary(_, inner) => needs_lazy(inner),
        Expr::Binary(_, l, r) => needs_lazy(l) || needs_lazy(r),
        Expr::Call(name, args) => {
            name.eq_ignore_ascii_case("hashfiles") || args.iter().any(needs_lazy)
        }
    }
}

/// `github.workspace`: runner-side truth only the step's environment knows.
fn workspace_leaf(expr: &Expr) -> bool {
    let Some((root, path)) = expr.dotted_path() else {
        return false;
    };
    root.eq_ignore_ascii_case("github")
        && matches!(path.as_slice(), [key] if key.eq_ignore_ascii_case("workspace"))
}

/// `runner.temp`: the same runner-side truth as `github.workspace`.
fn runner_temp_leaf(expr: &Expr) -> bool {
    let Some((root, path)) = expr.dotted_path() else {
        return false;
    };
    root.eq_ignore_ascii_case("runner")
        && matches!(path.as_slice(), [key] if key.eq_ignore_ascii_case("temp"))
}

/// `runner.tool_cache`: runner-side truth again, resolved by the step.
fn runner_tool_cache_leaf(expr: &Expr) -> bool {
    let Some((root, path)) = expr.dotted_path() else {
        return false;
    };
    root.eq_ignore_ascii_case("runner")
        && matches!(path.as_slice(), [key] if key.eq_ignore_ascii_case("tool_cache"))
}

/// `env.NAME` (or `env['NAME']`), the reference a step resolves itself.
fn env_leaf(expr: &Expr) -> Option<String> {
    let (root, path) = expr.dotted_path()?;
    if root.eq_ignore_ascii_case("env")
        && let [name] = path.as_slice()
    {
        return Some(name.to_string());
    }
    None
}

/// Lower one parsed condition onto a gate tree.
///
/// Operators split; a subtree with nothing lazy in it lowers whole to one engine
/// expression leaf; `env.NAME` and literal-pattern `hashFiles` become their step
/// leaves. Something lazy under a non-operator falls to the engine-leaf path,
/// which keeps `env` at its engine meaning (the scope env) and rejects
/// `hashFiles` there. `None` means a diagnostic was reported.
pub fn condition_tree(
    ast: &Expr,
    site: &Site,
    span: &Span,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<Gate> {
    if let Expr::Literal(lit) = ast {
        return Some(Gate::Lit(literal_value(lit)));
    }
    if !needs_lazy(ast) {
        return engine_leaf(ast, site, span, table, diags);
    }
    if let Some(name) = env_leaf(ast) {
        // The engine's view of the same reference is the fallback: the step's
        // environment wins, the scope env answers for workflow-declared values.
        let or = engine_leaf(ast, site, span, table, diags)?;
        return Some(Gate::Env {
            name,
            or: Some(Box::new(or)),
        });
    }
    if workspace_leaf(ast) {
        // A string leaf the step rewrites to its own workspace path before
        // evaluation; comparisons decompose around it (`needs_lazy`), so the
        // engine never evaluates over the marker.
        return Some(Gate::Lit(Value::String(
            crate::exprs::WORKSPACE_SENTINEL.to_string(),
        )));
    }
    if runner_temp_leaf(ast) {
        // The same shape for `runner.temp`: the step rewrites the marker to
        // its own `RUNNER_TEMP` before evaluation.
        return Some(Gate::Lit(Value::String(
            crate::exprs::RUNNER_TEMP_SENTINEL.to_string(),
        )));
    }
    if runner_tool_cache_leaf(ast) {
        // And for `runner.tool_cache`: the step rewrites the marker to the
        // tool cache it resolved for its environment before evaluation.
        return Some(Gate::Lit(Value::String(
            crate::exprs::RUNNER_TOOL_CACHE_SENTINEL.to_string(),
        )));
    }
    match ast {
        Expr::Group(inner) => condition_tree(inner, site, span, table, diags),
        Expr::Unary(UnaryOp::Not, arg) => {
            let arg = condition_tree(arg, site, span, table, diags)?;
            Some(Gate::Op {
                op: GateOp::Not,
                args: vec![arg],
            })
        }
        Expr::Binary(op, l, r) => {
            let l = condition_tree(l, site, span, table, diags)?;
            let r = condition_tree(r, site, span, table, diags)?;
            Some(Gate::Op {
                op: gate_op(*op),
                args: vec![l, r],
            })
        }
        Expr::Call(name, args) if name.eq_ignore_ascii_case("hashfiles") => {
            let patterns = literal_hashfiles_patterns(args, span, diags)?;
            Some(Gate::Lit(Value::String(hashfiles_sentinel(&patterns))))
        }
        // Something lazy sits under a non-operator (`contains(env.X, 'y')`,
        // `format('{0}', hashFiles(...))`). The engine-leaf path decides: `env`
        // keeps its engine-side meaning there, `hashFiles` is rejected.
        _ => engine_leaf(ast, site, span, table, diags),
    }
}

fn gate_op(op: frontend::expr::BinaryOp) -> GateOp {
    use frontend::expr::BinaryOp as B;
    match op {
        B::Eq => GateOp::Eq,
        B::Ne => GateOp::Ne,
        B::Lt => GateOp::Lt,
        B::Le => GateOp::Le,
        B::Gt => GateOp::Gt,
        B::Ge => GateOp::Ge,
        B::And => GateOp::And,
        B::Or => GateOp::Or,
    }
}

/// Lower a whole subtree to one engine expression leaf. Secrets are rejected —
/// gates carry no secret leaves — and so is a `hashFiles` that reached here (it
/// sits under something the engine would evaluate over the raw sentinel).
fn engine_leaf(
    ast: &Expr,
    site: &Site,
    span: &Span,
    table: &mut ExprTable,
    diags: &mut Diagnostics,
) -> Option<Gate> {
    let lowered = lower_expr(ast, site, true, span, table, diags)?;
    if lowered.saw_secret {
        diags.unsupported(
            "secrets.expression",
            span.clone(),
            "a `secrets.*` or `github.token` reference in a condition",
            "secrets are absent from the expression environment by construction, so they never reach the \
             event log; pass the secret through an environment variable and test it in the step \
             (`env.NAME`), but a condition cannot read the secret itself",
        );
        return None;
    }
    if lowered.saw_hashfiles {
        diags.unsupported(
            "expression.hashFiles",
            span.clone(),
            "a `hashFiles()` call under a function the engine evaluates",
            "in a condition, `hashFiles(...)` may stand alone or under the comparison and boolean \
             operators, where the step resolves it; under other functions the engine would \
             evaluate over the unresolved sentinel",
        );
        return None;
    }
    if lowered.saw_workspace {
        diags.unsupported(
            "expression.workspace",
            span.clone(),
            "`github.workspace` under a function the engine evaluates",
            "the workspace path is known only to the step's environment; in a condition, \
             `github.workspace` may stand alone or under the comparison and boolean operators, \
             where the step resolves it — or read `GITHUB_WORKSPACE` in the step itself",
        );
        return None;
    }
    if lowered.saw_runner_temp {
        diags.unsupported(
            "expression.runner_temp",
            span.clone(),
            "`runner.temp` under a function the engine evaluates",
            "the temp path is known only to the step's environment; in a condition, \
             `runner.temp` may stand alone or under the comparison and boolean operators, \
             where the step resolves it — or read `RUNNER_TEMP` in the step itself",
        );
        return None;
    }
    if lowered.saw_runner_tool_cache {
        diags.unsupported(
            "expression.runner_tool_cache",
            span.clone(),
            "`runner.tool_cache` under a function the engine evaluates",
            "the tool cache path is known only to the step's environment; in a condition, \
             `runner.tool_cache` may stand alone or under the comparison and boolean operators, \
             where the step resolves it — or read `RUNNER_TOOL_CACHE` in the step itself",
        );
        return None;
    }
    Some(Gate::expr(lowered.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Result<Option<Value>, ()> {
        Ok(None)
    }

    #[test]
    fn the_wire_format_round_trips() {
        let gate = Gate::Op {
            op: GateOp::And,
            args: vec![
                Gate::Lit(json!(true)),
                Gate::Op {
                    op: GateOp::Ne,
                    args: vec![
                        Gate::Env {
                            name: "HAS_TOKEN".into(),
                            or: Some(Box::new(Gate::Lit(Value::Null))),
                        },
                        Gate::Lit(json!("")),
                    ],
                },
            ],
        };
        let value = gate.to_value();
        assert_eq!(Gate::from_value(&value).unwrap(), gate);
        assert!(gate.reads_env());

        assert!(Gate::from_value(&json!({"op": "??", "args": []})).is_err());
        assert!(Gate::from_value(&json!({"op": "!", "args": []})).is_err());
        assert!(Gate::from_value(&json!("bare")).is_err());
        // A literal that *looks* like an operator node stays a literal.
        let tricky = Gate::Lit(json!({"op": "&&", "args": []}));
        assert_eq!(Gate::from_value(&tricky.to_value()).unwrap(), tricky);
    }

    #[test]
    fn operators_follow_github_semantics() {
        use ir::expr::builtins::loose;
        // `&&`/`||` return operands; comparisons are loose and case-insensitive.
        let and = Gate::Op {
            op: GateOp::And,
            args: vec![Gate::Lit(json!("x")), Gate::Lit(json!("y"))],
        };
        assert_eq!(eval(&and, &mut no_env).unwrap(), json!("y"));
        let or = Gate::Op {
            op: GateOp::Or,
            args: vec![Gate::Lit(json!("")), Gate::Lit(json!("fallback"))],
        };
        assert_eq!(eval(&or, &mut no_env).unwrap(), json!("fallback"));
        let eq = Gate::Op {
            op: GateOp::Eq,
            args: vec![Gate::Lit(json!("Linux")), Gate::Lit(json!("linux"))],
        };
        assert_eq!(eval(&eq, &mut no_env).unwrap(), json!(true));
        let lt = Gate::Op {
            op: GateOp::Lt,
            args: vec![Gate::Lit(json!("3")), Gate::Lit(json!(10))],
        };
        assert_eq!(eval(&lt, &mut no_env).unwrap(), json!(true));
        let not = Gate::Op {
            op: GateOp::Not,
            args: vec![Gate::Lit(json!(""))],
        };
        assert_eq!(eval(&not, &mut no_env).unwrap(), json!(true));
        // An unbound env leaf takes its fallback, else null.
        let env = Gate::Env {
            name: "MISSING".into(),
            or: Some(Box::new(Gate::Lit(json!("default")))),
        };
        assert_eq!(eval(&env, &mut no_env).unwrap(), json!("default"));
        let bare = Gate::Env {
            name: "MISSING".into(),
            or: None,
        };
        assert!(!loose::truthy(&eval(&bare, &mut no_env).unwrap()));
        // A bound one wins over the fallback.
        let mut bound = |name: &str| -> Result<Option<Value>, ()> {
            Ok((name == "MISSING").then(|| json!("set")))
        };
        assert_eq!(eval(&env, &mut bound).unwrap(), json!("set"));
    }
}
