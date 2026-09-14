//! `[run.model.fallbacks]`: reading Fabro's model-keyed fallback chains from
//! the settings layers. The reference grammar and the chains' place on the
//! lowered graph are [`frontend_attractor::fallbacks`]; this module checks
//! what Fabro's parser checks: the shape of the table, the shape of every
//! reference, and that a key does not name a provider-qualified model.

use std::collections::BTreeMap;

use frontend::{Diagnostics, Span};
pub use frontend_attractor::fallbacks::{DIAGNOSTIC, ModelRef, ParseModelRefError};

/// Read the `fallbacks` value of `[run.model]`. Every problem is an error
/// on `path`; a valid table comes back with each chain's references in
/// their canonical spelling (`provider:selector` for the legacy `/` form).
pub fn read(
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

#[cfg(test)]
mod tests {
    use super::*;

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
