//! The outputs-file protocol.
//!
//! A step writes `key=value` lines, or GHA's heredoc form, to the file named by
//! `CI_OUTPUT`. After the step exits the file is parsed into `Outcome.output`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    #[error("line {line}: expected `key=value` or `key<<DELIMITER`, got `{text}`")]
    Malformed { line: usize, text: String },
    #[error("line {line}: heredoc for `{key}` was never closed with `{delimiter}`")]
    UnclosedHeredoc {
        line: usize,
        key: String,
        delimiter: String,
    },
    #[error("line {line}: empty key")]
    EmptyKey { line: usize },
}

/// The failure class recorded when the outputs file cannot be read.
pub const BAD_OUTPUT_CLASS: &str = "bad_output_file";

/// Parse an outputs file into a flat object.
pub fn parse(text: &str) -> Result<Map<String, Value>, OutputError> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;

    while index < lines.len() {
        let raw = lines[index];
        let line_number = index + 1;
        index += 1;

        // Blank lines and comments are ignored, so a step can write a readable file.
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }

        if let Some((key, delimiter)) = raw.split_once("<<") {
            let key = key.trim();
            let delimiter = delimiter.trim();
            if key.is_empty() {
                return Err(OutputError::EmptyKey { line: line_number });
            }
            let mut body: Vec<&str> = Vec::new();
            let mut closed = false;
            while index < lines.len() {
                let candidate = lines[index];
                index += 1;
                if candidate.trim_end() == delimiter {
                    closed = true;
                    break;
                }
                body.push(candidate);
            }
            if !closed {
                return Err(OutputError::UnclosedHeredoc {
                    line: line_number,
                    key: key.to_string(),
                    delimiter: delimiter.to_string(),
                });
            }
            out.insert(key.to_string(), Value::String(body.join("\n")));
            continue;
        }

        let Some((key, value)) = raw.split_once('=') else {
            return Err(OutputError::Malformed {
                line: line_number,
                text: raw.to_string(),
            });
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(OutputError::EmptyKey { line: line_number });
        }
        out.insert(key.to_string(), Value::String(value.to_string()));
    }

    Ok(out.into_iter().collect())
}
