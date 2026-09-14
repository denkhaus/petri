//! Agent context compaction: the settings a native agent session runs with.
//!
//! The pinned Fabro exposes no configuration for compaction. Its agent
//! sessions run with hardcoded values (`fabro-agent`'s `SessionOptions`):
//! compaction on, a trigger at 80 percent of the model's context window, and
//! the six most recent turns kept verbatim. `[run.agent]` refuses a
//! `compaction` key, and so does Petri (`lower::workflow_toml`). These values
//! are Fabro's meaning of "compaction", so Petri lowers them onto every agent
//! node as the `compaction` config object, and the native backend translates
//! them into Pebble's options. Pebble owns the estimate, the trigger, the
//! safe cut, the summary call and the history replacement. This is separate
//! from workflow fidelity (`lower::threads`), whose `compact` mode is a
//! deterministic preamble and never a model call.

use serde_json::{Value, json};

/// Fabro's compaction trigger: the share of the model's context window the
/// estimated active context (system prompt and history) must exceed.
pub const DEFAULT_THRESHOLD_PERCENT: u8 = 80;

/// Fabro's default for how many recent turns compaction leaves untouched.
pub const DEFAULT_PRESERVE_TURNS: u32 = 6;

/// The `compaction` object on an agent node's step config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionSettings {
    /// Whether history is summarized as it approaches the context window.
    pub enabled:           bool,
    /// The share of the context window, in whole percent, above which
    /// compaction runs. Fabro compares with "greater than".
    pub threshold_percent: u8,
    /// How many recent turns stay verbatim.
    pub preserve_turns:    u32,
}

impl Default for CompactionSettings {
    /// Fabro's hardcoded session values.
    fn default() -> Self {
        Self {
            enabled:           true,
            threshold_percent: DEFAULT_THRESHOLD_PERCENT,
            preserve_turns:    DEFAULT_PRESERVE_TURNS,
        }
    }
}

impl CompactionSettings {
    /// The JSON form the agent step reads.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "threshold_percent": self.threshold_percent,
            "preserve_turns": self.preserve_turns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_fabros() {
        let settings = CompactionSettings::default();
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 80);
        assert_eq!(settings.preserve_turns, 6);
        assert_eq!(
            settings.to_json(),
            json!({"enabled": true, "threshold_percent": 80, "preserve_turns": 6})
        );
    }
}
