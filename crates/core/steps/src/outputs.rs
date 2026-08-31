//! The outputs-file protocol.
//!
//! A step writes `key=value` lines to the file named by `CI_OUTPUT`; a
//! multi-line value uses the heredoc form `key<<DELIM … DELIM`. After the step
//! exits the file is parsed into `Outcome.output`.
//!
//! The format is deliberately a superset of GitHub's `$GITHUB_OUTPUT` file so
//! that variable can be a plain alias of ours (see
//! `ProcessConfig.output_env_aliases`). That is a compatibility choice in this
//! step kind, not something the engine knows.

use std::collections::BTreeMap;

use ir::FailureClass;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    #[error("line {line}: expected `key=value` or `key<<DELIMITER`, got `{text}`")]
    Malformed { line: usize, text: String },
    #[error("line {line}: heredoc for `{key}` was never closed with `{delimiter}`")]
    UnclosedHeredoc {
        line:      usize,
        key:       String,
        delimiter: String,
    },
    #[error("line {line}: empty key")]
    EmptyKey { line: usize },
    #[error("line {line}: heredoc for `{key}` names no delimiter")]
    EmptyDelimiter { line: usize, key: String },
}

/// The failure class recorded when the outputs file cannot be read.
pub const BAD_OUTPUT_CLASS: FailureClass = FailureClass::new_static("bad_output_file");

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

        // Whichever of `=` and `<<` comes first decides the form, as GitHub's
        // runner does: `URL=a<<b` is a plain pair, `K<<EOF` a heredoc even
        // when its body carries `=`.
        let is_heredoc = match (raw.find('='), raw.find("<<")) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(equals), Some(heredoc)) => heredoc < equals,
        };
        if is_heredoc {
            let (key, delimiter) = raw.split_once("<<").expect("the marker was found");
            let key = key.trim();
            let delimiter = delimiter.trim();
            if key.is_empty() {
                return Err(OutputError::EmptyKey { line: line_number });
            }
            if delimiter.is_empty() {
                return Err(OutputError::EmptyDelimiter {
                    line: line_number,
                    key:  key.to_string(),
                });
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
                    line:      line_number,
                    key:       key.to_string(),
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

/// Pinned against actions/runner's `EnvFileKeyValuePairs`
/// (`src/Runner.Worker/FileCommandManager.cs`), the parser behind
/// `GITHUB_ENV`, `GITHUB_OUTPUT` and `GITHUB_STATE`. Where this parser is
/// deliberately more lenient than the runner, the test says so.
#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Vec<(String, String)> {
        parse(text)
            .expect("parses")
            .into_iter()
            .map(|(k, v)| (k, v.as_str().expect("a string").to_string()))
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn plain_pairs_and_the_last_duplicate_wins() {
        assert_eq!(ok("A=1\nB=two\nA=3\n"), pairs(&[("A", "3"), ("B", "two")]));
    }

    #[test]
    fn only_the_first_equals_splits() {
        // The runner splits on the first `=` (`Split('=', 2)`).
        assert_eq!(
            ok("A=b=c\nURL=https://h/p?q=1&r=2\nEMPTY=\n"),
            pairs(&[("A", "b=c"), ("EMPTY", ""), ("URL", "https://h/p?q=1&r=2")])
        );
    }

    #[test]
    fn an_equals_before_the_heredoc_marker_is_a_plain_pair() {
        // The runner dispatches on whichever of `=` and `<<` comes first; a
        // value containing `<<` is a value, and nothing after it is swallowed.
        assert_eq!(
            ok("DIFF=a<<b\nNEXT=still-here\n"),
            pairs(&[("DIFF", "a<<b"), ("NEXT", "still-here")])
        );
        // The other way round, `=` inside a heredoc header is part of the
        // delimiter.
        assert_eq!(ok("K<<X=Y\nvalue\nX=Y\n"), pairs(&[("K", "value")]));
    }

    #[test]
    fn a_heredoc_round_trips_a_multiline_value() {
        assert_eq!(
            ok("MULTI<<EOF\nline one\nkey=value inside\n\nline four\nEOF\nAFTER=1\n"),
            pairs(&[
                ("AFTER", "1"),
                ("MULTI", "line one\nkey=value inside\n\nline four"),
            ])
        );
        // An empty body is the empty string, as the runner's
        // `endIndex > startIndex ? … : string.Empty`.
        assert_eq!(ok("E<<EOF\nEOF\n"), pairs(&[("E", "")]));
    }

    #[test]
    fn a_heredoc_closes_only_on_an_exact_line() {
        // The runner compares each line to the delimiter with
        // `string.Equals(..., Ordinal)`: a line that merely contains it, or
        // carries it with a prefix, is body.
        assert_eq!(
            ok("K<<EOF\nEOF and more\nxEOF\n EOF\nEOFEOF\nEOF\n"),
            pairs(&[("K", "EOF and more\nxEOF\n EOF\nEOFEOF")])
        );
    }

    #[test]
    fn an_unclosed_heredoc_is_an_error() {
        assert_eq!(
            parse("A=1\nK<<EOF\nbody\nmore\n"),
            Err(OutputError::UnclosedHeredoc {
                line:      2,
                key:       "K".into(),
                delimiter: "EOF".into(),
            })
        );
    }

    #[test]
    fn an_empty_delimiter_is_an_error() {
        // The runner: "Invalid format 'K<<'. Name must not be empty and
        // delimiter must not be empty". A blank line after it does not close
        // anything, and no value is produced.
        assert_eq!(
            parse("K<<\n\nvalue\n\n"),
            Err(OutputError::EmptyDelimiter {
                line: 1,
                key:  "K".into(),
            })
        );
        assert_eq!(
            parse("K<<   \nEOF\n"),
            Err(OutputError::EmptyDelimiter {
                line: 1,
                key:  "K".into(),
            })
        );
    }

    #[test]
    fn an_empty_key_is_an_error() {
        assert_eq!(
            parse("<<EOF\nbody\nEOF\n"),
            Err(OutputError::EmptyKey { line: 1 })
        );
        // The runner means to reject `=value` too ("Name must not be empty")
        // but its check tests the whole line rather than the name, so it
        // accepts a pair keyed by the empty string. This parser rejects it:
        // the stricter side of an accident.
        assert_eq!(
            parse("A=1\n=value\n"),
            Err(OutputError::EmptyKey { line: 2 })
        );
    }

    #[test]
    fn a_line_with_neither_separator_is_malformed() {
        assert_eq!(
            parse("A=1\njust words\n"),
            Err(OutputError::Malformed {
                line: 2,
                text: "just words".into(),
            })
        );
    }

    #[test]
    fn crlf_lines_read_as_their_values() {
        // A deliberate lenience: the runner's `\r\n` handling is
        // `#if OS_WINDOWS`, so its Linux build keeps the `\r` in the value
        // (and in a heredoc's delimiter). Here a CRLF file reads exactly as
        // its LF twin would, on every platform.
        assert_eq!(
            ok("A=one\r\nK<<EOF\r\nline 1\r\nline 2\r\nEOF\r\nB=two\r\n"),
            pairs(&[("A", "one"), ("B", "two"), ("K", "line 1\nline 2")])
        );
    }

    #[test]
    fn blank_and_comment_lines_are_skipped() {
        // Blank lines are skipped by the runner too. Whitespace-only lines and
        // `#` comments are this parser's lenience — the runner has no comment
        // syntax and rejects both as "Invalid format" — kept so a step can
        // write a readable file.
        assert_eq!(
            ok("\n# a comment\nA=1\n   \n  # indented comment\nB=2\n\n"),
            pairs(&[("A", "1"), ("B", "2")])
        );
    }

    #[test]
    fn utf8_keys_and_values_round_trip() {
        assert_eq!(
            ok("GREETING=héllo wörld ✓\nκλειδί=τιμή\nK<<✂\n日本語\n✂\n"),
            pairs(&[
                ("GREETING", "héllo wörld ✓"),
                ("K", "日本語"),
                ("κλειδί", "τιμή"),
            ])
        );
    }

    #[test]
    fn a_missing_final_newline_still_reads() {
        assert_eq!(ok("A=1\nB=2"), pairs(&[("A", "1"), ("B", "2")]));
        assert_eq!(ok("K<<EOF\nbody\nEOF"), pairs(&[("K", "body")]));
        assert_eq!(ok(""), pairs(&[]));
    }

    #[test]
    fn surrounding_whitespace_on_keys_and_delimiters_is_trimmed() {
        // A lenience over the runner, which keeps a key's whitespace and
        // matches the delimiter byte for byte. Values keep theirs.
        assert_eq!(
            ok(" A = spaced \nK<< EOF \nbody\nEOF  \n"),
            pairs(&[("A", " spaced "), ("K", "body")])
        );
    }
}
