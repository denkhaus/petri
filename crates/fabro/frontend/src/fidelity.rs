//! Fabro's fidelity modes and thread identities, as pure rules.
//!
//! Fidelity says how much of the run so far an agent or prompt node hears
//! about before its own prompt. A thread names the conversation a `full`
//! fidelity node continues. Both resolve at run time from the edge the token
//! arrived on, the node, and the graph; the lowering carries every input and
//! the step applies these rules. Fabro's own resolution lives in
//! `fabro-workflow/src/lifecycle/fidelity.rs` at the pinned revision.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much prior context a node receives. `compact` is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Fidelity {
    /// No preamble: the node continues its thread's conversation.
    #[serde(rename = "full")]
    Full,
    /// Only the goal and the run id.
    #[serde(rename = "truncate")]
    Truncate,
    /// The nested-bullet summary of completed stages.
    #[default]
    #[serde(rename = "compact")]
    Compact,
    /// A brief summary: the last two stages, about 600 tokens.
    #[serde(rename = "summary:low")]
    SummaryLow,
    /// A moderate summary: the last five stages, about 1,500 tokens.
    #[serde(rename = "summary:medium")]
    SummaryMedium,
    /// A detailed per-stage report.
    #[serde(rename = "summary:high")]
    SummaryHigh,
}

impl Fidelity {
    /// Every mode, in the order the diagnostics list them.
    pub const ALL: &'static [Self] = &[
        Self::Full,
        Self::Truncate,
        Self::Compact,
        Self::SummaryLow,
        Self::SummaryMedium,
        Self::SummaryHigh,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Truncate => "truncate",
            Self::Compact => "compact",
            Self::SummaryLow => "summary:low",
            Self::SummaryMedium => "summary:medium",
            Self::SummaryHigh => "summary:high",
        }
    }

    /// `full` becomes `summary:high`; every other mode stays. Fabro applies
    /// this to a resumed node whose conversation is gone, and to a parallel
    /// branch, whose concurrent siblings cannot share one session.
    #[must_use]
    pub const fn degraded(self) -> Self {
        match self {
            Self::Full => Self::SummaryHigh,
            other => other,
        }
    }

    /// Fabro's resolution: the incoming edge, then the node, then the graph
    /// default, then `compact`.
    pub fn resolve(edge: Option<Self>, node: Option<Self>, graph: Option<Self>) -> Self {
        edge.or(node).or(graph).unwrap_or_default()
    }

    /// The mode a parallel branch's first node runs at: the fork edge, then
    /// the branch node, each degraded; `None` inherits the fork's preamble.
    pub fn resolve_branch(edge: Option<Self>, node: Option<Self>) -> Option<Self> {
        edge.or(node).map(Self::degraded)
    }
}

impl fmt::Display for Fidelity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text was not a fidelity mode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a fidelity mode")]
pub struct UnknownFidelity(pub String);

impl FromStr for Fidelity {
    type Err = UnknownFidelity;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|mode| mode.as_str() == text)
            .ok_or_else(|| UnknownFidelity(text.to_owned()))
    }
}

/// Fabro's thread resolution: the incoming edge's `thread_id`, then the
/// node's, then the graph's `default_thread`, then the node's first class,
/// then the previous node's id.
pub fn resolve_thread<'a>(
    edge: Option<&'a str>,
    node: Option<&'a str>,
    graph: Option<&'a str>,
    first_class: Option<&'a str>,
    previous: Option<&'a str>,
) -> Option<&'a str> {
    edge.or(node).or(graph).or(first_class).or(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_exactly_and_print_back() {
        for mode in Fidelity::ALL {
            assert_eq!(mode.as_str().parse::<Fidelity>(), Ok(*mode));
            assert_eq!(mode.to_string(), mode.as_str());
        }
        assert!("Full".parse::<Fidelity>().is_err());
        assert!("summary".parse::<Fidelity>().is_err());
    }

    #[test]
    fn resolution_prefers_the_edge_then_the_node_then_the_graph() {
        assert_eq!(
            Fidelity::resolve(
                Some(Fidelity::Truncate),
                Some(Fidelity::Full),
                Some(Fidelity::SummaryLow)
            ),
            Fidelity::Truncate
        );
        assert_eq!(
            Fidelity::resolve(None, Some(Fidelity::Full), Some(Fidelity::SummaryLow)),
            Fidelity::Full
        );
        assert_eq!(
            Fidelity::resolve(None, None, Some(Fidelity::SummaryLow)),
            Fidelity::SummaryLow
        );
        assert_eq!(Fidelity::resolve(None, None, None), Fidelity::Compact);
    }

    #[test]
    fn a_branch_degrades_full_and_inherits_when_unset() {
        assert_eq!(
            Fidelity::resolve_branch(Some(Fidelity::Full), None),
            Some(Fidelity::SummaryHigh)
        );
        assert_eq!(
            Fidelity::resolve_branch(None, Some(Fidelity::SummaryLow)),
            Some(Fidelity::SummaryLow)
        );
        assert_eq!(Fidelity::resolve_branch(None, None), None);
    }

    #[test]
    fn threads_fall_through_to_the_class_and_the_previous_node() {
        assert_eq!(
            resolve_thread(Some("e"), Some("n"), Some("g"), Some("c"), Some("p")),
            Some("e")
        );
        assert_eq!(
            resolve_thread(None, None, None, Some("impl"), Some("plan")),
            Some("impl")
        );
        assert_eq!(resolve_thread(None, None, None, None, Some("plan")), Some("plan"));
        assert_eq!(resolve_thread(None, None, None, None, None), None);
    }
}
