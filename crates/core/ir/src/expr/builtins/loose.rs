//! Loose coercion: the JavaScript-family rules, as pure functions over JSON
//! values.
//!
//! These back the `loose_*` builtins. A frontend whose expression language
//! compares loosely — GitHub Actions does, and most YAML CI dialects follow it
//! — lowers its operators onto them; the frontend evaluates nothing. One
//! evaluator.
//!
//! The rules:
//!
//! | Type    | As a number                                          |
//! |---------|------------------------------------------------------|
//! | null    | 0                                                    |
//! | boolean | 1 / 0                                                |
//! | string  | trimmed; empty is 0; a decimal with optional sign, `0x` hex, `0o` octal, or `Infinity` / `NaN`; else NaN |
//! | array   | NaN                                                  |
//! | object  | NaN                                                  |
//!
//! Equality: same kinds compare directly (strings case-insensitively; arrays
//! and objects only equal by identity, so never here). Different kinds both
//! coerce to a number. NaN is never equal to anything, and any relational
//! comparison involving NaN is false.
//!
//! Falsy: `false`, `0`, `-0`, `""`, `null`, `NaN`. Empty arrays and objects are
//! **truthy** — the opposite of this crate's own `is_truthy`, which is why the
//! two are separate functions rather than one with a flag.
//!
//! On string-to-number: the reference implementations parse with a leading
//! sign, leading zeros and a trailing point allowed (`+1`, `01`, `1.`), which
//! is looser than JSON's grammar. That is what this follows, since it is what
//! real expressions were written against.
//!
//! On string-from-container: a container has no loose string form. It renders
//! as its type name (`Array`, `Object`) so that a value that should have been a
//! scalar shows up as a visible mistake rather than as a JSON blob silently
//! reaching a command line.

use std::cmp::Ordering;

use serde_json::Value;

/// Coerce a value to a number by the loose rules.
pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Null | Value::Bool(false) => 0.0,
        Value::Bool(true) => 1.0,
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => parse_number(s),
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn parse_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        "NaN" => return f64::NAN,
        _ => {}
    }
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let sign = if negative { -1.0 } else { 1.0 };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).map_or(f64::NAN, |n| sign * n as f64);
    }
    if let Some(oct) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8).map_or(f64::NAN, |n| sign * n as f64);
    }
    if !looks_like_decimal(body) {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// .NET's `AllowDecimalPoint | AllowExponent` shape: digits with an optional
/// fraction (either side may be empty, but not both) and an optional exponent.
/// No whitespace inside, no thousands separators, no second sign.
fn looks_like_decimal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
}

/// Coerce a value to a string by the loose rules: `null` is empty, booleans are
/// lowercase, numbers print integral when they are integral, arrays and objects
/// print as their type names.
pub fn to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(_) => "Array".to_string(),
        Value::Object(_) => "Object".to_string(),
    }
}

/// How a coerced number prints. Integral values drop the decimal point, so
/// `format('{0}', 3)` is `3`, not `3.0`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the guard proves `n` is integral and below 1e21, far inside `i128`"
)]
pub fn format_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// Loose truthiness. Note that empty arrays and objects are truthy.
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn is_same_kind(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::Null, Value::Null)
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_))
    )
}

/// Loose `==`.
#[expect(
    clippy::float_cmp,
    reason = "the loose rules define `==` as exact numeric equality after coercion; a \
              tolerance would change which values compare equal"
)]
pub fn is_equal(a: &Value, b: &Value) -> bool {
    if is_same_kind(a, b) {
        return match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Number(_), Value::Number(_)) => {
                let (x, y) = (to_number(a), to_number(b));
                !x.is_nan() && x == y
            }
            (Value::String(x), Value::String(y)) => {
                x.eq_ignore_ascii_case(y) || x.to_lowercase() == y.to_lowercase()
            }
            // Arrays and objects are equal only by identity, which values never are.
            _ => false,
        };
    }
    let (x, y) = (to_number(a), to_number(b));
    !x.is_nan() && x == y
}

/// Loose relational comparison. Two strings compare case-insensitively as
/// strings; anything else coerces to numbers, and NaN makes every comparison
/// false.
pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    if let (Value::String(x), Value::String(y)) = (a, b) {
        return Some(x.to_lowercase().cmp(&y.to_lowercase()));
    }
    let (x, y) = (to_number(a), to_number(b));
    x.partial_cmp(&y)
}

/// Case-insensitive property lookup.
pub fn get_ci<'a>(object: &'a Value, key: &str) -> Option<&'a Value> {
    let map = object.as_object()?;
    if let Some(exact) = map.get(key) {
        return Some(exact);
    }
    let lowered = key.to_lowercase();
    map.iter()
        .find(|(k, _)| k.to_lowercase() == lowered)
        .map(|(_, v)| v)
}
