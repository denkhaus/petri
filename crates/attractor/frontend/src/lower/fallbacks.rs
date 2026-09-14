//! `[run.model.fallbacks]`: Fabro's model-keyed fallback chains.
//!
//! A chain is keyed by a requested model (`"gpt-sol" = [...]`) and lists
//! model references in the order to try when a request on that model fails
//! with a provider-local error. Each reference is a bare token (a provider
//! name or a model selector; only a catalog can tell which), a
//! `provider:selector` pair, or the legacy `provider/selector` form. The
//! parser follows Fabro's `ModelRef`: whichever separator appears first
//! decides, a `/` before any `:` is the legacy pair, and a `:` token stays
//! bare because model ids legitimately contain colons.
//!
//! Lowering keeps the chains as written (the Fabro frontend reads and checks
//! the table; `frontend_fabro::fallbacks`). Canonicalizing keys, filtering
//! targets against the configured providers and mapping reasoning effort all
//! need the model catalog, which the runner has and the frontend does not, so
//! that happens in the steps (`attractor_steps::fallback`) as Fabro resolves at
//! run start on its server. What the frontend does check is what Fabro's
//! parser checks: the shape of the table, the shape of every reference, and
//! that a key does not name a provider-qualified model.

use std::fmt;
use std::str::FromStr;

use serde_json::{Map, Value, json};

use super::settings::RunSettings;

/// The agent and prompt config key the chains ride on: an object keyed by
/// the requested model, each value the reference list as written.
pub const CONFIG_KEY: &str = "fallbacks";

/// The diagnostic code every `[run.model.fallbacks]` problem carries.
pub const DIAGNOSTIC: &str = "fabro.model_fallbacks";

/// One parsed model reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelRef {
    /// A bare token: a provider name, a model alias, or a model id.
    Bare(String),
    /// A provider-qualified model selector.
    Qualified { provider: String, selector: String },
}

/// Why a model reference did not parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseModelRefError {
    #[error("model reference is empty")]
    Empty,
    #[error(
        "model reference {input:?}: qualify it as \"provider:selector\" when the selector \
         contains \"/\""
    )]
    TooManySlashes { input: String },
    #[error("model reference {input:?}: provider and selector sides must both be non-empty")]
    EmptySide { input: String },
}

impl FromStr for ModelRef {
    type Err = ParseModelRefError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ParseModelRefError::Empty);
        }
        let (provider, selector) = match (trimmed.find('/'), trimmed.find(':')) {
            (Some(slash), colon) if colon.is_none_or(|colon| slash < colon) => {
                (&trimmed[..slash], &trimmed[slash + 1..])
            }
            _ => return Ok(Self::Bare(trimmed.to_owned())),
        };
        if selector.contains('/') {
            return Err(ParseModelRefError::TooManySlashes {
                input: input.to_owned(),
            });
        }
        if provider.is_empty() || selector.is_empty() {
            return Err(ParseModelRefError::EmptySide {
                input: input.to_owned(),
            });
        }
        Ok(Self::Qualified {
            provider: provider.to_owned(),
            selector: selector.to_owned(),
        })
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bare(token) => f.write_str(token),
            Self::Qualified { provider, selector } => write!(f, "{provider}:{selector}"),
        }
    }
}

impl ModelRef {
    /// Promote a bare `prefix:rest` token to a qualified reference when
    /// `is_provider(prefix)` holds. Fabro's `ModelRef::qualify`.
    #[must_use]
    pub fn qualify(self, is_provider: impl Fn(&str) -> bool) -> Self {
        match self {
            Self::Bare(token) => match token.split_once(':') {
                Some((prefix, rest))
                    if !prefix.is_empty() && !rest.is_empty() && is_provider(prefix) =>
                {
                    Self::Qualified {
                        provider: prefix.to_owned(),
                        selector: rest.to_owned(),
                    }
                }
                _ => Self::Bare(token),
            },
            qualified @ Self::Qualified { .. } => qualified,
        }
    }
}

/// Put the run's chains on an LLM node's config, when there are any.
pub(super) fn write(settings: &RunSettings, config: &mut Map<String, Value>) {
    if settings.model.fallbacks.is_empty() {
        return;
    }
    config.insert(CONFIG_KEY.into(), json!(settings.model.fallbacks));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_as_fabro_parses_them() {
        assert_eq!(
            "gpt-sol".parse::<ModelRef>(),
            Ok(ModelRef::Bare("gpt-sol".into()))
        );
        // A colon token stays bare until a catalog says the prefix is a provider.
        assert_eq!(
            "anthropic:claude-opus".parse::<ModelRef>(),
            Ok(ModelRef::Bare("anthropic:claude-opus".into()))
        );
        assert_eq!(
            "ollama/llama:8b".parse::<ModelRef>(),
            Ok(ModelRef::Qualified {
                provider: "ollama".into(),
                selector: "llama:8b".into(),
            })
        );
        assert_eq!(
            "a/b/c".parse::<ModelRef>(),
            Err(ParseModelRefError::TooManySlashes {
                input: "a/b/c".into(),
            })
        );
        assert_eq!(
            "/model".parse::<ModelRef>(),
            Err(ParseModelRefError::EmptySide {
                input: "/model".into(),
            })
        );
        assert_eq!("  ".parse::<ModelRef>(), Err(ParseModelRefError::Empty));
        assert_eq!(
            "openai/gpt-5.5".parse::<ModelRef>().map(|r| r.to_string()),
            Ok("openai:gpt-5.5".to_owned())
        );
    }

    #[test]
    fn qualify_promotes_a_provider_prefix_only() {
        let is_provider = |name: &str| name == "anthropic";
        assert_eq!(
            ModelRef::Bare("anthropic:claude-opus".into()).qualify(is_provider),
            ModelRef::Qualified {
                provider: "anthropic".into(),
                selector: "claude-opus".into(),
            }
        );
        assert_eq!(
            ModelRef::Bare("llama:8b".into()).qualify(is_provider),
            ModelRef::Bare("llama:8b".into())
        );
        assert_eq!(
            ModelRef::Bare("anthropic".into()).qualify(is_provider),
            ModelRef::Bare("anthropic".into())
        );
    }
}
