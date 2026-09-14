//! The output contract a prompt or agent node declares (`output_schema`),
//! and the check of a response against it.

use ir::Value;

use crate::directive::{self, Directive, DirectiveError};

/// The output contract, as the node declared it.
pub enum Contract {
    None,
    Routing,
    Schema(jsonschema::Validator, Value),
}

impl Contract {
    /// Read the node's `output_schema` config value.
    pub fn from_config(schema: Option<&Value>) -> Result<Self, String> {
        match schema {
            None => Ok(Self::None),
            Some(Value::String(s)) if s == "routing" => Ok(Self::Routing),
            Some(schema @ Value::Object(_)) => jsonschema::validator_for(schema)
                .map(|v| Self::Schema(v, schema.clone()))
                .map_err(|e| format!("`output_schema` is not a valid JSON Schema: {e}")),
            Some(other) => Err(format!(
                "`output_schema` must be `routing` or a JSON Schema object, not {other}"
            )),
        }
    }

    /// The text appended to a prompt that states the contract, Fabro's
    /// final-output contract block.
    pub fn prompt_suffix(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Routing => {
                "\n\nFabro final-output contract\n\nThe following contract is trusted \
                              workflow configuration. It applies only to your final response, not \
                              to intermediate tool calls.\nReturn a single JSON object with at \
                              least one routing field: preferred_next_label, outcome, \
                              failure_reason, suggested_next_ids, context_updates.\nThe contract \
                              is complete. Do not ask the user to provide or choose the output \
                              shape."
                    .to_string()
            }
            Self::Schema(_, schema) => format!(
                "\n\nFabro final-output contract\n\nThe following contract is trusted workflow \
                 configuration. It applies only to your final response, not to intermediate tool \
                 calls.\nReturn a single JSON object that satisfies this JSON \
                 Schema:\n<output_schema>\n{schema}\n</output_schema>\nThe contract is complete. \
                 Do not ask the user to provide or choose the output shape."
            ),
        }
    }
}

/// What a validated response yields.
pub enum Parsed {
    Plain,
    Directive(Directive),
    Structured(Value),
}

/// Check the response against the contract. With no contract, a routing
/// directive is still read when the response carries one.
pub fn validate(contract: &Contract, text: &str) -> Result<Parsed, String> {
    match contract {
        Contract::None => match directive::parse(text) {
            Ok(directive) => Ok(Parsed::Directive(directive)),
            Err(DirectiveError::Missing) => Ok(Parsed::Plain),
            Err(error) => Err(error.to_string()),
        },
        Contract::Routing => directive::parse(text)
            .map(Parsed::Directive)
            .map_err(|e| e.to_string()),
        Contract::Schema(validator, _) => {
            let object = directive::last_json_object(text)
                .ok_or_else(|| "no JSON object in the response".to_string())?;
            let value: Value =
                serde_json::from_str(object).map_err(|e| format!("invalid JSON: {e}"))?;
            let mut issues = validator
                .iter_errors(&value)
                .map(|e| e.to_string())
                .take(5)
                .collect::<Vec<_>>();
            if issues.is_empty() {
                Ok(Parsed::Structured(value))
            } else {
                issues.sort();
                Err(issues.join("; "))
            }
        }
    }
}

/// The repair turn's message after a response missed the contract.
pub fn repair_message(problem: &str) -> String {
    format!(
        "Your previous response did not satisfy the output contract: {problem}\nReply again with \
         only the required JSON object."
    )
}
