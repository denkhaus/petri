//! Handoff §7 test 1: the expression grammar, property-tested.
//!
//! Parse → print → reparse round-trips; malformed text yields an error and never a
//! panic; precedence is GitHub's; the two lowerings emit only table functions.

use frontend::expr::lower::{EngineBindings, strict};
use frontend::expr::{BinaryOp, Expr, Segment, parse, print, split_template};
use ir::ExprTable;
use proptest::prelude::*;

fn parses_to(source: &str) -> Expr {
    parse(source).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

#[test]
fn literals() {
    assert_eq!(parses_to("null"), Expr::null());
    assert_eq!(parses_to("true"), Expr::boolean(true));
    assert_eq!(parses_to("false"), Expr::boolean(false));
    assert_eq!(parses_to("711"), Expr::number(711.0));
    assert_eq!(parses_to("-9.2"), Expr::number(-9.2));
    assert_eq!(parses_to("-2.99e-2"), Expr::number(-2.99e-2));
    assert_eq!(parses_to("0xff"), Expr::number(255.0));
    assert_eq!(parses_to("1e3"), Expr::number(1000.0));
    assert_eq!(parses_to("'hello'"), Expr::string("hello"));
    assert_eq!(
        parses_to("'it''s'"),
        Expr::string("it's"),
        "`''` is the only escape"
    );
    assert_eq!(parses_to("''"), Expr::string(""));
}

#[test]
fn property_and_index_access() {
    assert_eq!(
        parses_to("github.event_name"),
        Expr::ident("github").property("event_name")
    );
    assert_eq!(
        parses_to("github['event_name']"),
        Expr::Index(
            Box::new(Expr::ident("github")),
            Box::new(Expr::string("event_name"))
        )
    );
    assert_eq!(
        parses_to("matrix.fail-fast"),
        Expr::ident("matrix").property("fail-fast"),
        "property names may contain `-`"
    );
    assert_eq!(
        parses_to("steps.*.outcome"),
        Expr::Wildcard(Box::new(Expr::ident("steps"))).property("outcome")
    );
    assert_eq!(
        parses_to("a[*]"),
        Expr::Wildcard(Box::new(Expr::ident("a")))
    );
    assert_eq!(
        parses_to("a[0]"),
        Expr::Index(Box::new(Expr::ident("a")), Box::new(Expr::number(0.0)))
    );
    // Keywords are valid after a dot.
    assert_eq!(parses_to("a.true"), Expr::ident("a").property("true"));
}

/// GitHub's precedence table: `!` > relational > equality > `&&` > `||`.
#[test]
fn precedence_matches_github() {
    let e = parses_to("a || b && c");
    assert!(
        matches!(e, Expr::Binary(BinaryOp::Or, _, _)),
        "`&&` binds tighter than `||`: {e}"
    );

    let e = parses_to("a && b == c");
    assert!(
        matches!(e, Expr::Binary(BinaryOp::And, _, _)),
        "`==` binds tighter than `&&`: {e}"
    );

    let e = parses_to("a == b < c");
    assert!(
        matches!(e, Expr::Binary(BinaryOp::Eq, _, _)),
        "`<` binds tighter than `==`: {e}"
    );

    let e = parses_to("!a == b");
    assert!(
        matches!(e, Expr::Binary(BinaryOp::Eq, _, _)),
        "`!` binds tightest: {e}"
    );
    if let Expr::Binary(_, l, _) = e {
        assert!(matches!(*l, Expr::Unary(..)));
    }

    // Postfix binds tighter than `!`.
    let e = parses_to("!a.b");
    assert!(matches!(e, Expr::Unary(_, ref inner) if matches!(**inner, Expr::Property(..))));

    // Equality is left-associative; `||` and `&&` too.
    let e = parses_to("a == b != c");
    assert!(
        matches!(e, Expr::Binary(BinaryOp::Ne, ref l, _) if matches!(**l, Expr::Binary(BinaryOp::Eq, ..)))
    );

    // Relational does not chain.
    assert!(parse("a < b < c").is_err());
    assert!(parse("(a < b) < c").is_ok());
}

#[test]
fn calls() {
    assert_eq!(
        parses_to("contains(github.ref, 'main')"),
        Expr::call(
            "contains",
            vec![Expr::ident("github").property("ref"), Expr::string("main")]
        )
    );
    assert_eq!(parses_to("always()"), Expr::call("always", vec![]));
    assert_eq!(
        parses_to("format('{0}-{1}', a, b)"),
        Expr::call(
            "format",
            vec![Expr::string("{0}-{1}"), Expr::ident("a"), Expr::ident("b")]
        )
    );
}

#[test]
fn malformed_input_is_an_error_not_a_panic() {
    for bad in [
        "",
        "  ",
        "(",
        ")",
        "a.",
        "a[",
        "a[]",
        "'unterminated",
        "a = b",
        "a & b",
        "a | b",
        "==",
        "&& a",
        "a &&",
        "1 2",
        "a b",
        "f(",
        "f(a,",
        ".",
        "-",
        "0x",
        "'a' 'b'",
        "a < b < c",
        "a.*.",
        "\u{1F600}",
    ] {
        assert!(parse(bad).is_err(), "`{bad}` should not parse");
    }
}

/// The parser has a depth cap, so pathological nesting is an error rather than a
/// stack overflow.
#[test]
fn deep_nesting_is_rejected_not_overflowed() {
    let deep = format!("{}a{}", "(".repeat(10_000), ")".repeat(10_000));
    assert!(parse(&deep).is_err());
    let deep_not = format!("{}a", "!".repeat(10_000));
    assert!(parse(&deep_not).is_err());
}

#[test]
fn templates_split_into_segments() {
    assert_eq!(
        split_template("pre-${{ a.b }}-post").unwrap(),
        vec![
            Segment::Text("pre-".into()),
            Segment::Expr {
                source: " a.b ".into(),
                offset: 7
            },
            Segment::Text("-post".into()),
        ]
    );
    assert_eq!(
        split_template("plain").unwrap(),
        vec![Segment::Text("plain".into())]
    );
    assert_eq!(split_template("").unwrap(), Vec::<Segment>::new());
    assert_eq!(
        split_template("${{ a }}${{ b }}").unwrap(),
        vec![
            Segment::Expr {
                source: " a ".into(),
                offset: 3
            },
            Segment::Expr {
                source: " b ".into(),
                offset: 11
            },
        ]
    );
    assert_eq!(split_template("x ${{ never closed"), Err(2));
}

/// The strict lowering only ever emits functions the engine's table has.
#[test]
fn strict_lowering_emits_only_table_functions() {
    let sources = [
        "a == b",
        "a != b && c < d || !e",
        "a.b.c",
        "a['b'][0]",
        "a.*.b",
        "(a || b) && c",
        "len(a) > 0 && contains(a, 'x')",
        "always() || success()",
    ];
    for source in sources {
        let ast = parses_to(source);
        let mut table = ExprTable::new();
        strict(&ast, &mut table, &mut EngineBindings).unwrap_or_else(|e| panic!("`{source}`: {e}"));
        for (_, expr) in table.iter() {
            if let ir::Expr::Call(fname, args) = expr {
                let spec = ir::expr::builtin(fname).unwrap_or_else(|| {
                    panic!("`{source}` emitted `{fname}`, which is not in BUILTINS")
                });
                assert_eq!(spec.arity, args.len(), "`{fname}` arity in `{source}`");
            }
        }
    }
    // A function the table does not have is an error, not a call.
    let ast = parses_to("hashFiles('x')");
    let mut table = ExprTable::new();
    assert!(strict(&ast, &mut table, &mut EngineBindings).is_err());
}

// ── Properties ────────────────────────────────────────────────────────────

fn arb_ident() -> impl Strategy<Value = String> {
    "[a-zA-Z_][a-zA-Z0-9_-]{0,8}".prop_filter("not a keyword", |s| {
        !matches!(s.as_str(), "null" | "true" | "false")
    })
}

fn arb_literal() -> impl Strategy<Value = Expr> {
    prop_oneof![
        Just(Expr::null()),
        any::<bool>().prop_map(Expr::boolean),
        // Finite doubles that print and reparse exactly.
        (-1.0e12f64..1.0e12).prop_map(|n| Expr::number((n * 1000.0).round() / 1000.0)),
        (0i64..100000).prop_map(|n| Expr::number(n as f64)),
        "[ -~]{0,12}".prop_map(|s| Expr::string(&s)),
    ]
}

/// Postfix operators bind tighter than `!` and every binary operator, so the parser
/// can only ever produce a postfix node whose base is itself postfix-able. A
/// generator that put `!a` under `.b` would be asking for a tree no source can mean.
fn postfixable(e: Expr) -> Expr {
    match e {
        Expr::Unary(..) | Expr::Binary(..) => Expr::Group(Box::new(e)),
        other => other,
    }
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![arb_literal(), arb_ident().prop_map(Expr::Ident)];
    leaf.prop_recursive(5, 48, 4, |inner| {
        prop_oneof![
            (inner.clone(), arb_ident())
                .prop_map(|(b, n)| Expr::Property(Box::new(postfixable(b)), n)),
            (inner.clone(), inner.clone())
                .prop_map(|(b, k)| Expr::Index(Box::new(postfixable(b)), Box::new(k))),
            inner
                .clone()
                .prop_map(|b| Expr::Wildcard(Box::new(postfixable(b)))),
            inner.clone().prop_map(|b| Expr::Unary(
                frontend::expr::UnaryOp::Not,
                Box::new(Expr::Group(Box::new(b)))
            )),
            (
                prop_oneof![
                    Just(BinaryOp::Lt),
                    Just(BinaryOp::Le),
                    Just(BinaryOp::Gt),
                    Just(BinaryOp::Ge),
                    Just(BinaryOp::Eq),
                    Just(BinaryOp::Ne),
                    Just(BinaryOp::And),
                    Just(BinaryOp::Or),
                ],
                inner.clone(),
                inner.clone()
            )
                .prop_map(|(op, l, r)| Expr::Binary(
                    op,
                    Box::new(Expr::Group(Box::new(l))),
                    Box::new(Expr::Group(Box::new(r)))
                )),
            (arb_ident(), prop::collection::vec(inner.clone(), 0..3))
                .prop_map(|(n, a)| Expr::Call(n, a)),
            inner.prop_map(|b| Expr::Group(Box::new(b))),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(600))]

    /// Parse → print → reparse is the identity on the tree.
    #[test]
    fn print_then_parse_round_trips(expr in arb_expr()) {
        let printed = print(&expr);
        let reparsed = parse(&printed).unwrap_or_else(|e| panic!("`{printed}`: {e}"));
        prop_assert_eq!(reparsed, expr);
    }

    /// Printing what was parsed prints the same thing again: the printer is stable.
    #[test]
    fn print_is_idempotent(expr in arb_expr()) {
        let once = print(&expr);
        let twice = print(&parse(&once).unwrap());
        prop_assert_eq!(once, twice);
    }

    /// Arbitrary text never panics the parser, whatever it is.
    #[test]
    fn arbitrary_text_never_panics(text in "\\PC{0,64}") {
        let _ = parse(&text);
        let _ = split_template(&text);
    }

    /// Arbitrary bytes, lossily decoded, never panic the parser either.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let text = String::from_utf8_lossy(&bytes);
        let _ = parse(&text);
    }

    /// Expression-shaped noise: fragments of real syntax glued together at random.
    #[test]
    fn syntax_fragments_never_panic(parts in prop::collection::vec(
        prop_oneof![
            Just("("), Just(")"), Just("["), Just("]"), Just("."), Just("*"), Just(","),
            Just("!"), Just("<"), Just("<="), Just("=="), Just("!="), Just("&&"), Just("||"),
            Just("'"), Just("''"), Just("a"), Just("b1"), Just("0x"), Just("1e"), Just("-"),
            Just("null"), Just("true"), Just(" "), Just("${{"), Just("}}"),
        ], 0..24)) {
        let text: String = parts.concat();
        let _ = parse(&text);
        let _ = split_template(&text);
    }

    /// Whatever parses, lowers to expressions whose calls are all in the table. A
    /// lowering can fail on an unknown function, but it never panics and never
    /// smuggles a call past the gate.
    #[test]
    fn whatever_parses_lowers_onto_the_table(expr in arb_expr()) {
        let mut table = ExprTable::new();
        if strict(&expr, &mut table, &mut EngineBindings).is_ok() {
            for (_, e) in table.iter() {
                if let ir::Expr::Call(name, _) = e {
                    prop_assert!(ir::expr::builtin(name).is_some(), "`{name}` escaped the table");
                }
            }
        }
    }
}
