//! Print an expression back to source.
//!
//! `parse(print(e)) == e` for every `e` the parser produces: the property tests
//! hold the printer to it. Groups are printed as written, so precedence never
//! has to be re-derived.

use super::ast::{Expr, Literal};

pub fn print(expr: &Expr) -> String {
    let mut out = String::new();
    write(expr, &mut out);
    out
}

fn write(expr: &Expr, out: &mut String) {
    match expr {
        Expr::Literal(Literal::Null) => out.push_str("null"),
        Expr::Literal(Literal::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        Expr::Literal(Literal::Number(n)) => out.push_str(&print_number(*n)),
        Expr::Literal(Literal::Str(s)) => {
            out.push('\'');
            out.push_str(&s.replace('\'', "''"));
            out.push('\'');
        }
        Expr::Ident(name) => out.push_str(name),
        Expr::Property(base, name) => {
            write(base, out);
            out.push('.');
            out.push_str(name);
        }
        Expr::Index(base, key) => {
            write(base, out);
            out.push('[');
            write(key, out);
            out.push(']');
        }
        Expr::Wildcard(base) => {
            write(base, out);
            out.push_str(".*");
        }
        Expr::Unary(_, inner) => {
            out.push('!');
            write(inner, out);
        }
        Expr::Binary(op, l, r) => {
            write(l, out);
            out.push(' ');
            out.push_str(op.symbol());
            out.push(' ');
            write(r, out);
        }
        Expr::Call(name, args) => {
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(a, out);
            }
            out.push(')');
        }
        Expr::Group(inner) => {
            out.push('(');
            write(inner, out);
            out.push(')');
        }
    }
}

/// Numbers print in a form the lexer reads back to the same double.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the cast runs only where `n` is integral and under 1e15 in magnitude, a range i64 holds exactly"
)]
fn print_number(n: f64) -> String {
    if n.is_nan() {
        // Not expressible as a literal; the closest total answer.
        return "0".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "1e999" } else { "-1e999" }.to_string();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        // `{:?}` on f64 is the shortest round-tripping form, and always has a `.`
        // or an exponent, so it never collides with an integer literal.
        format!("{n:?}")
    }
}
