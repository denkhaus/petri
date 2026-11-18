//! Fork presence pin (denkhaus/work#1312 → denkhaus/work#1300 fix A):
//! question ids are URL-safe. A `just vendor import` from a wrong fork
//! state rewrites the tree cleanly and would silently drop the fork
//! module — this file is the pin that fails when that happens.

use attractor_steps::fork_url_safe;

/// The two producer shapes (the human gate's and the agent question's)
/// contain no character a proxy rejects or rewrites in a path segment.
#[test]
fn question_ids_carry_no_url_unsafe_byte() {
    let gate = fork_url_safe::id(&["gate", "12"]);
    let agent = fork_url_safe::id(&["planner", "3", "agent", "ses_x", "call_y", "0"]);

    assert_eq!(gate, "gate~12");
    assert_eq!(agent, "planner~3~agent~ses_x~call_y~0");
    for id in [gate, agent] {
        for forbidden in ['#', '/', '%', ' ', '?'] {
            assert!(
                !id.contains(forbidden),
                "question id carries URL-unsafe {forbidden:?}: {id}"
            );
        }
    }
}

/// The join is injective even when components carry the separator or the
/// historical reserved characters — the pending-answer map keys on the id.
#[test]
fn components_with_reserved_characters_stay_distinct() {
    assert_eq!(
        fork_url_safe::id(&["gate#3", "/agent/x"]),
        "gate~233~~2Fagent~2Fx"
    );
    assert_ne!(
        fork_url_safe::id(&["a~b", "c"]),
        fork_url_safe::id(&["a", "b~c"])
    );
}
