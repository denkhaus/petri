//! Acceptance §7 item 1: arbitrary text never panics the frontend; it only
//! diagnoses.

use frontend_attractor::{condition, load_text};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn arbitrary_text_never_panics(text in "\\PC{0,200}") {
        let _ = load_text("fuzz.fabro", &text);
    }

    #[test]
    fn arbitrary_graph_bodies_never_panic(body in "[a-z_ \\[\\]=\"{}>,;\\n-]{0,160}") {
        let text = format!("digraph G {{ start [shape=Mdiamond] exit [shape=Msquare] {body} }}");
        let _ = load_text("fuzz.fabro", &text);
    }

    #[test]
    fn arbitrary_conditions_never_panic(cond in "[a-z_.=!<>&| \"0-9]{0,40}") {
        let _ = condition::parse(&cond);
        let text = format!(
            "digraph G {{ start [shape=Mdiamond] exit [shape=Msquare] a [prompt=\"x\"] b [prompt=\"x\"] start -> a a -> exit [condition=\"{}\"] a -> b b -> exit }}",
            cond.replace('"', "'")
        );
        let _ = load_text("fuzz.fabro", &text);
    }
}
