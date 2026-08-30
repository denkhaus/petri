//! The built-in function table, and the one place each entry is implemented.
//!
//! [`BUILTINS`] gates dispatch: [`eval_call`] looks a name up here before any
//! match arm is reached, so a function missing from the table is unknown
//! however many arms exist, and an entry with no arm fails its own conformance
//! test. The two cannot drift. [`loose`] and [`combine`] hold the pure value
//! functions the entries call.

pub mod combine;
pub mod loose;

use std::cmp::Ordering;

use serde_json::Value;
use smol_str::SmolStr;

use super::ExprTable;
use super::eval::{
    EvalEnv, EvalError, concat, eval_at, index_into, num, to_display, truthy, type_err,
};
use crate::ids::ExprId;

/// One entry in the built-in function table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Builtin {
    pub name:    &'static str,
    pub arity:   usize,
    pub summary: &'static str,
}

/// Every function an expression may call.
///
/// This table is not documentation that sits beside the implementation — it
/// **gates** it. [`eval`](super::eval) looks a call up here before dispatching,
/// so a function that is not in the table is unknown however many match arms
/// exist, and an entry with no arm fails its own conformance test. The two
/// cannot drift.
///
/// # The bar for adding one
///
/// A language that grows a function whenever a test needs one becomes an ad-hoc
/// scripting language by accretion. Every entry must be:
///
/// - **pure** — no IO, no clock, no randomness, no ambient state;
/// - **total** — every input either yields a value or a typed [`EvalError`],
///   never a panic;
/// - **tested** — in `crates/core/ir/tests/expressions.rs`, including its error
///   cases;
/// - **necessary** — justified by a test or a frontend mapping that genuinely
///   cannot be written without it. `split` met this bar: the outputs-file
///   protocol yields strings, and `for_each` needs an array.
///
/// This is also the surface a frontend expression grammar maps onto, so
/// additions widen a contract rather than adding a convenience.
pub const BUILTINS: &[Builtin] = &[
    // Status predicates. `status` is bound wherever an outcome is in scope.
    Builtin {
        name:    "always",
        arity:   0,
        summary: "true, whatever the status",
    },
    Builtin {
        name:    "never",
        arity:   0,
        summary: "false, whatever the status",
    },
    Builtin {
        name:    "success",
        arity:   0,
        summary: "success-like: Success or PartialSuccess (see Status::is_success_like)",
    },
    Builtin {
        name:    "partial_success",
        arity:   0,
        summary: "exactly PartialSuccess",
    },
    Builtin {
        name:    "full_success",
        arity:   0,
        summary: "exactly Success, excluding PartialSuccess",
    },
    Builtin {
        name:    "failure",
        arity:   0,
        summary: "exactly Failure",
    },
    Builtin {
        name:    "skipped",
        arity:   0,
        summary: "exactly Skipped",
    },
    Builtin {
        name:    "cancelled",
        arity:   0,
        summary: "exactly Cancelled",
    },
    Builtin {
        name:    "timed_out",
        arity:   0,
        summary: "exactly TimedOut",
    },
    // Values.
    Builtin {
        name:    "not",
        arity:   1,
        summary: "logical negation, by truthiness",
    },
    Builtin {
        name:    "len",
        arity:   1,
        summary: "length of an array, object or string",
    },
    Builtin {
        name:    "to_string",
        arity:   1,
        summary: "render a value as a string",
    },
    Builtin {
        name:    "default",
        arity:   2,
        summary: "the first value unless it is null, else the second",
    },
    Builtin {
        name:    "get",
        arity:   2,
        summary: "index an array by number or an object by key",
    },
    Builtin {
        name:    "contains",
        arity:   2,
        summary: "membership in an array, object keys, or a substring",
    },
    // Full `regex` semantics, unanchored search, compiled per evaluation: a frontend
    // that validates patterns at parse time must never have the core reject one at
    // runtime, which is why this is `regex` and not `regex-lite`.
    Builtin {
        name:    "matches",
        arity:   2,
        summary: "regex search in a string",
    },
    Builtin {
        name:    "concat",
        arity:   2,
        summary: "join two arrays, strings or objects",
    },
    // Lists. These exist so a collector can reassemble expansion results, and so a
    // step's string output can feed `for_each`, without the language needing lambdas.
    Builtin {
        name:    "split",
        arity:   2,
        summary: "split a string on a separator, dropping empty pieces",
    },
    Builtin {
        name:    "sort_by_key",
        arity:   2,
        summary: "order an array of objects by one field",
    },
    Builtin {
        name:    "pluck",
        arity:   2,
        summary: "take one field from every object in an array",
    },
    // Loose semantics: the JavaScript-family coercion rules most CI expression
    // languages share. A frontend whose format compares loosely lowers its operators
    // onto these instead of `Eq` / `Lt`. Rules are in [`loose`].
    Builtin {
        name:    "loose_eq",
        arity:   2,
        summary: "loose `==`: differing kinds coerce to numbers; strings compare case-insensitively",
    },
    Builtin {
        name:    "loose_lt",
        arity:   2,
        summary: "loose `<`; false whenever a side coerces to NaN",
    },
    Builtin {
        name:    "loose_le",
        arity:   2,
        summary: "loose `<=`",
    },
    Builtin {
        name:    "loose_gt",
        arity:   2,
        summary: "loose `>`",
    },
    Builtin {
        name:    "loose_ge",
        arity:   2,
        summary: "loose `>=`",
    },
    Builtin {
        name:    "loose_truthy",
        arity:   1,
        summary: "loose truthiness: empty arrays and objects are truthy",
    },
    Builtin {
        name:    "loose_number",
        arity:   1,
        summary: "loose number coercion; NaN becomes null",
    },
    Builtin {
        name:    "loose_string",
        arity:   1,
        summary: "loose string coercion: null is empty, containers render as their type name",
    },
    Builtin {
        name:    "contains_ci",
        arity:   2,
        summary: "array membership by loose equality, or case-insensitive substring",
    },
    Builtin {
        name:    "starts_with",
        arity:   2,
        summary: "case-insensitive prefix test over string coercions",
    },
    Builtin {
        name:    "ends_with",
        arity:   2,
        summary: "case-insensitive suffix test over string coercions",
    },
    Builtin {
        name:    "format",
        arity:   2,
        summary: "positional `{N}` substitution from an array of arguments; `{{` and `}}` are literal braces",
    },
    Builtin {
        name:    "join",
        arity:   2,
        summary: "join an array with a separator, coercing each element to a string",
    },
    Builtin {
        name:    "to_json",
        arity:   1,
        summary: "pretty-printed JSON",
    },
    Builtin {
        name:    "from_json",
        arity:   1,
        summary: "parse a JSON string; a non-string passes through",
    },
    // Records and lists of records.
    Builtin {
        name:    "get_ci",
        arity:   2,
        summary: "case-insensitive property lookup",
    },
    Builtin {
        name:    "values",
        arity:   1,
        summary: "an object's values, or an array itself",
    },
    Builtin {
        name:    "pluck_present",
        arity:   2,
        summary: "one field from every record that has it; records without it are dropped",
    },
    Builtin {
        name:    "keys",
        arity:   1,
        summary: "an object's keys, in order",
    },
    Builtin {
        name:    "omit",
        arity:   2,
        summary: "an object without the named keys",
    },
    Builtin {
        name:    "cartesian",
        arity:   1,
        summary: "every combination of one value per key of an object of arrays",
    },
    Builtin {
        name:    "reject_where",
        arity:   2,
        summary: "drop every record matching any of the partial records",
    },
    Builtin {
        name:    "extend_where",
        arity:   3,
        summary: "merge partial records into compatible records, protecting the named keys; append the rest",
    },
];

/// Look a function up in the table.
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

pub(super) fn eval_call(
    table: &ExprTable,
    name: &SmolStr,
    args: &[ExprId],
    env: &EvalEnv<'_>,
    depth: u32,
) -> Result<Value, EvalError> {
    // The table gates dispatch: an unknown name never reaches a match arm, and
    // arity is checked once, here, rather than in nineteen places.
    let spec = builtin(name).ok_or_else(|| EvalError::UnknownFunction(name.clone()))?;
    if args.len() != spec.arity {
        return Err(EvalError::Arity {
            name:     name.clone(),
            expected: spec.arity,
            got:      args.len(),
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
        "matches" => {
            let (text, pattern) = (arg(0)?, arg(1)?);
            let text = text
                .as_str()
                .ok_or_else(|| type_err("matches", "a string", &text))?;
            let pattern = pattern
                .as_str()
                .ok_or_else(|| type_err("matches", "a string pattern", &pattern))?;
            // Unanchored search, like `Regex::is_match` everywhere: a pattern that
            // wants anchoring writes its own `^` and `$`. Compiled per evaluation —
            // deterministic and pure; a cache would need interior mutability the
            // table deliberately does not have.
            let re = regex::Regex::new(pattern).map_err(|e| EvalError::Type {
                op:       SmolStr::new("matches"),
                expected: SmolStr::new("a valid regex pattern"),
                got:      SmolStr::new(format!("an invalid pattern ({e})")),
            })?;
            Ok(Value::Bool(re.is_match(text)))
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
                        .unwrap_or(Ordering::Equal),
                    (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
                    _ => Ordering::Equal,
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
        "loose_eq" => Ok(Value::Bool(loose::equal(&arg(0)?, &arg(1)?))),
        "loose_lt" | "loose_le" | "loose_gt" | "loose_ge" => {
            let ord = loose::compare(&arg(0)?, &arg(1)?);
            Ok(Value::Bool(match (name.as_str(), ord) {
                (_, None) => false,
                ("loose_lt", Some(o)) => o.is_lt(),
                ("loose_le", Some(o)) => o.is_le(),
                ("loose_gt", Some(o)) => o.is_gt(),
                (_, Some(o)) => o.is_ge(),
            }))
        }
        "loose_truthy" => Ok(Value::Bool(loose::truthy(&arg(0)?))),
        "loose_number" => Ok(num(loose::to_number(&arg(0)?))),
        "loose_string" => Ok(Value::String(loose::to_string(&arg(0)?))),
        "contains_ci" => {
            let (search, item) = (arg(0)?, arg(1)?);
            Ok(Value::Bool(match &search {
                Value::Array(items) => items.iter().any(|i| loose::equal(i, &item)),
                Value::Object(_) => false,
                primitive => match &item {
                    Value::Array(_) | Value::Object(_) => false,
                    _ => loose::to_string(primitive)
                        .to_lowercase()
                        .contains(&loose::to_string(&item).to_lowercase()),
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
            let text = loose::to_string(&text).to_lowercase();
            let probe = loose::to_string(&probe).to_lowercase();
            Ok(Value::Bool(if name == "starts_with" {
                text.starts_with(&probe)
            } else {
                text.ends_with(&probe)
            }))
        }
        "format" => {
            let (template, args_value) = (arg(0)?, arg(1)?);
            let template = loose::to_string(&template);
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
                loose::to_string(&separator)
            };
            Ok(Value::String(match &items {
                Value::Array(items) => items
                    .iter()
                    .map(loose::to_string)
                    .collect::<Vec<_>>()
                    .join(&separator),
                other => loose::to_string(other),
            }))
        }
        "to_json" => Ok(Value::String(
            serde_json::to_string_pretty(&arg(0)?).unwrap_or_default(),
        )),
        "from_json" => {
            let v = arg(0)?;
            match &v {
                Value::String(s) => serde_json::from_str::<Value>(s).map_err(|e| EvalError::Type {
                    op:       SmolStr::new("from_json"),
                    expected: SmolStr::new("valid JSON"),
                    got:      SmolStr::new(format!("invalid JSON ({e})")),
                }),
                _ => Ok(v),
            }
        }
        "get_ci" => {
            let (object, key) = (arg(0)?, arg(1)?);
            let Some(key) = key.as_str() else {
                return Ok(Value::Null);
            };
            Ok(loose::get_ci(&object, key).cloned().unwrap_or(Value::Null))
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
                    .filter_map(|item| loose::get_ci(item, key).cloned())
                    .collect(),
            ))
        }
        "keys" => Ok(Value::Array(combine::keys(&arg(0)?))),
        "omit" => Ok(combine::omit(&arg(0)?, &arg(1)?)),
        "cartesian" => Ok(Value::Array(combine::cartesian(&arg(0)?))),
        "reject_where" => Ok(Value::Array(combine::reject_where(&arg(0)?, &arg(1)?))),
        "extend_where" => Ok(Value::Array(combine::extend_where(
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

/// Positional formatting: `{N}` substitutes argument N, `{{` and `}}` are
/// literal braces. An index with no argument is an error rather than an empty
/// string.
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
                        op:       SmolStr::new("format"),
                        expected: SmolStr::new("`{N}` placeholders"),
                        got:      SmolStr::new("a malformed placeholder"),
                    });
                }
                let index: usize = digits.parse().unwrap_or(usize::MAX);
                match args.get(index) {
                    Some(v) => out.push_str(&loose::to_string(v)),
                    None => {
                        return Err(EvalError::Type {
                            op:       SmolStr::new("format"),
                            expected: SmolStr::new(format!("at least {} argument(s)", index + 1)),
                            got:      SmolStr::new(format!("{}", args.len())),
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
