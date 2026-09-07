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
//! Lowering keeps the chains as written. Canonicalizing keys, filtering
//! targets against the configured providers and mapping reasoning effort all
//! need the model catalog, which the runner has and the frontend does not, so
//! that happens in the steps (`fabro_steps::fallback`) as Fabro resolves at
//! run start on its server. What the frontend does check is what Fabro's
//! parser checks: the shape of the table, the shape of every reference, and
//! that a key does not name a provider-qualified model.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use frontend::{Diagnostics, Span};
use serde_json::{Map, Value, json};

use super::workflow_toml::RunSettings;

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

/// Read the `fallbacks` value of `[run.model]`. Every problem is an error
/// on `path`; a valid table comes back with each chain's references in
/// their canonical spelling (`provider:selector` for the legacy `/` form).
pub(super) fn read(
    path: &str,
    diags: &mut Diagnostics,
    value: &toml::Value,
) -> BTreeMap<String, Vec<String>> {
    let span = Span::file(path);
    let Some(table) = value.as_table() else {
        diags.error(
            DIAGNOSTIC,
            span,
            format!(
                "`run.model.fallbacks` in `{path}` must be a table keyed by the requested model, \
                 as in `\"gpt-sol\" = [\"anthropic:claude-opus\"]`"
            ),
        );
        return BTreeMap::new();
    };
    let mut chains = BTreeMap::new();
    for (key, entries) in table {
        match key.parse::<ModelRef>() {
            Ok(ModelRef::Bare(_)) => {}
            Ok(ModelRef::Qualified { selector, .. }) => {
                diags.error(
                    DIAGNOSTIC,
                    span.clone(),
                    format!(
                        "`run.model.fallbacks` keys name a requested model; use `{selector}` \
                         instead of `{key}`"
                    ),
                );
                continue;
            }
            Err(error) => {
                diags.error(
                    DIAGNOSTIC,
                    span.clone(),
                    format!("`run.model.fallbacks` key: {error}"),
                );
                continue;
            }
        }
        let Some(list) = entries.as_array() else {
            diags.error(
                DIAGNOSTIC,
                span.clone(),
                format!(
                    "`run.model.fallbacks.{key}` in `{path}` must be a list of model references"
                ),
            );
            continue;
        };
        let mut references = Vec::with_capacity(list.len());
        for entry in list {
            let Some(text) = entry.as_str() else {
                diags.error(
                    DIAGNOSTIC,
                    span.clone(),
                    format!(
                        "`run.model.fallbacks.{key}` in `{path}` holds a non-string entry; each \
                         entry is a model reference such as `anthropic:claude-opus`"
                    ),
                );
                continue;
            };
            match text.parse::<ModelRef>() {
                Ok(reference) => references.push(reference.to_string()),
                Err(error) => diags.error(
                    DIAGNOSTIC,
                    span.clone(),
                    format!("`run.model.fallbacks.{key}` in `{path}`: {error}"),
                ),
            }
        }
        chains.insert(key.clone(), references);
    }
    chains
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

    #[test]
    fn read_keeps_chains_and_reports_every_shape_problem() {
        let mut diags = Diagnostics::default();
        let table: toml::Table = r#"
            "gpt-sol" = ["anthropic:claude-opus", "openrouter/gpt-5.5", "kimi-k3"]
            "openai/gpt-terra" = ["claude-opus"]
            "bad" = "not a list"
            "worse" = [1, "a/b/c", ""]
        "#
        .parse()
        .expect("toml");
        let value = toml::Value::Table(table);
        let chains = read("workflow.toml", &mut diags, &value);
        assert_eq!(
            chains.get("gpt-sol").map(Vec::as_slice),
            Some(
                [
                    "anthropic:claude-opus".to_owned(),
                    "openrouter:gpt-5.5".to_owned(),
                    "kimi-k3".to_owned(),
                ]
                .as_slice()
            )
        );
        assert!(!chains.contains_key("openai/gpt-terra"));
        assert!(!chains.contains_key("bad"));
        assert_eq!(chains.get("worse").map(Vec::len), Some(0));
        let messages: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
        assert!(diags.iter().all(|d| d.code == DIAGNOSTIC), "{messages:?}");
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|m| m.contains("use `gpt-terra` instead of `openai/gpt-terra`")),
            "{messages:?}"
        );
    }
}
