//! YAML with positions.
//!
//! Every node knows where it came from, so a diagnostic can point at the line.
//! This wraps `marked_yaml` in a small API shaped for what frontends do: look
//! up keys, iterate sequences, read scalars as the type the format expects, and
//! complain with a span when the shape is wrong.

use marked_yaml::Node as MarkedNode;
use marked_yaml::types::{MarkedMappingNode, MarkedScalarNode, MarkedSequenceNode};
use serde_json::Value;
use smol_str::SmolStr;

use crate::diag::{Diagnostics, Span};

mod loader;

/// A parsed document, with the file name every span will carry.
pub struct Document {
    file: SmolStr,
    root: MarkedNode,
}

impl Document {
    /// Parse a file's text. A syntax error becomes a `yaml.syntax` diagnostic
    /// rather than a panic or a bare `Err`.
    ///
    /// Coercion prevention is on: a quoted scalar (`''`, `'true'`, `'123'`)
    /// stays a string, so [`Scalar::is_plain`] really means "written
    /// unquoted" and only plain scalars type-infer. Anchors and aliases
    /// resolve while loading, each alias a copy of the anchored node that
    /// keeps the anchor site's spans.
    ///
    /// One scanner strictness is repaired rather than reported: a multi-line
    /// flow collection whose closing `]`/`}` sits at its key's indentation
    /// — which GitHub accepts — is re-indented (whitespace only, so nothing
    /// moves but the bracket) and parsed again. See [`pad_flow_close`].
    pub fn parse(file: &str, text: &str, diags: &mut Diagnostics) -> Option<Document> {
        let mut current = std::borrow::Cow::Borrowed(text);
        // Bounded: each repair pads one closer line, and a file has finitely many;
        // the bound only caps pathological input.
        for _ in 0..16 {
            let failure = match loader::load(&current) {
                Ok(root) => {
                    return Some(Document {
                        file: SmolStr::new(file),
                        root,
                    });
                }
                Err(failure) => failure,
            };
            let (mut line, mut column) = failure.position();
            let message = failure.message();
            // Some errors carry their position only in the text.
            if line == 0
                && let Some((l, c)) = position_in_message(&message)
            {
                line = l;
                column = c;
            }
            if message.contains("invalid indentation")
                && let Some(repaired) = pad_flow_close(&current, line)
            {
                current = std::borrow::Cow::Owned(repaired);
                continue;
            }
            if message.contains("invalid indentation")
                && multiline_flow_before(&current, line as usize)
            {
                // The underlying reader rejects a multi-line `[…]` or `{…}` value
                // followed by a dedent, which GitHub accepts. The closer-only shape
                // is repaired above; what reaches here is an under-indented flow
                // item — a known limitation of the positional YAML library, named
                // as such rather than blamed on the file.
                diags.unsupported(
                    "yaml.multiline_flow",
                    Span::new(file, line, column),
                    "a multi-line `[…]` or `{…}` value followed by a dedent",
                    "the positional YAML reader does not accept this shape yet; write the list in block \
                     form (`- item` per line) or on one line",
                );
            } else {
                diags.error(
                    "yaml.syntax",
                    Span::new(file, line, column),
                    format!("could not parse YAML: {message}"),
                );
            }
            return None;
        }
        diags.error(
            "yaml.syntax",
            Span::new(file, 0, 0),
            "could not parse YAML: flow-collection repair did not converge",
        );
        None
    }

    pub fn root(&self) -> Node<'_> {
        Node {
            file:  &self.file,
            inner: &self.root,
        }
    }

    pub fn file(&self) -> &str {
        &self.file
    }
}

/// `… line 67 column 9` → (67, 9).
fn position_in_message(message: &str) -> Option<(u32, u32)> {
    let after = message.split("line ").nth(1)?;
    let line: u32 = after.split_whitespace().next()?.parse().ok()?;
    let column: u32 = message
        .split("column ")
        .nth(1)
        .and_then(|c| c.split_whitespace().next())
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Some((line, column))
}

/// The scanner rejects a multi-line flow collection whose closing `]`/`}` line
/// sits at (or left of) the opening line's indentation — but only after a
/// quoted item, and GitHub accepts the shape everywhere. When the "invalid
/// indentation" error points at such a line, return the text with that one line
/// re-indented past its opener. Whitespace only: nothing moves but the bracket,
/// so every other span in the document stays put, and the caller re-parses to
/// verify. `None` when the error line is anything but a lone closer — a real
/// error.
fn pad_flow_close(text: &str, line: u32) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let idx = (line as usize).checked_sub(1)?;
    let current = *lines.get(idx)?;
    // Only a line that is nothing but the closer (with an optional comma or
    // comment after it) is a candidate; the scanner cannot be mid-scalar there.
    let body = current.trim();
    let after = body.strip_prefix([']', '}'])?;
    let after = after.strip_prefix(',').unwrap_or(after).trim_start();
    if !(after.is_empty() || after.starts_with('#')) {
        return None;
    }
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    // On the closer's line, scan only up to the closer itself, not a comment.
    let opener = flow_opener(&lines, idx, &current[..=indent_of(current)])?;
    if opener >= idx {
        return None;
    }
    let target = indent_of(lines[opener]) + 2;
    if indent_of(current) >= target {
        return None;
    }
    let mut out = lines;
    let padded = format!("{}{}", " ".repeat(target - indent_of(current)), current);
    out[idx] = &padded;
    Some(out.join("\n"))
}

/// The line that opened the flow collection whose closer sits on line `close`:
/// walk backwards from there, counting bracket depth in reversed character
/// order. `close_slice` is the part of the closing line to scan — a caller that
/// knows where the closer sits can exclude what follows it. Brackets inside
/// strings can miscount; every caller verifies (a re-parse, or only classifying
/// a diagnostic).
fn flow_opener(lines: &[&str], close: usize, close_slice: &str) -> Option<usize> {
    let mut depth = 0i32;
    for j in (0..=close).rev() {
        let slice = if j == close { close_slice } else { lines[j] };
        for c in slice.chars().rev() {
            match c {
                ']' | '}' => depth += 1,
                '[' | '{' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Whether the lines before `line` close a flow collection that opened on an
/// earlier line — the shape the underlying reader gets wrong.
fn multiline_flow_before(text: &str, line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    if line == 0 {
        return false;
    }
    // Start at the failing line itself — the reader often reports the closing
    // bracket — and look back a few non-blank lines from there.
    let mut i = line.min(lines.len());
    let mut looked = 0;
    while i > 0 && looked < 4 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        looked += 1;
        if (t.ends_with(']') || t.ends_with('}'))
            && let Some(j) = flow_opener(&lines, i, lines[i])
        {
            return j < i;
        }
    }
    false
}

/// A node in the document, with its file attached so it can produce spans.
#[derive(Clone, Copy)]
pub struct Node<'a> {
    file:  &'a str,
    inner: &'a MarkedNode,
}

impl<'a> Node<'a> {
    pub fn span(&self) -> Span {
        let start = self.inner.span().start();
        Span::new(
            self.file,
            start.map_or(0, |m| m.line() as u32),
            start.map_or(0, |m| m.column() as u32),
        )
    }

    pub fn as_mapping(&self) -> Option<Mapping<'a>> {
        self.inner.as_mapping().map(|m| Mapping {
            file:  self.file,
            inner: m,
            span:  self.span(),
        })
    }

    pub fn as_sequence(&self) -> Option<Sequence<'a>> {
        self.inner.as_sequence().map(|s| Sequence {
            file:  self.file,
            inner: s,
            span:  self.span(),
        })
    }

    pub fn as_scalar(&self) -> Option<Scalar<'a>> {
        self.inner.as_scalar().map(|s| Scalar {
            inner: s,
            span:  self.span(),
        })
    }

    /// The raw text of a scalar, whatever its type.
    pub fn as_str(&self) -> Option<&'a str> {
        self.inner.as_scalar().map(|s| s.as_str())
    }

    pub fn is_mapping(&self) -> bool {
        self.inner.as_mapping().is_some()
    }

    pub fn is_sequence(&self) -> bool {
        self.inner.as_sequence().is_some()
    }

    pub fn is_scalar(&self) -> bool {
        self.inner.as_scalar().is_some()
    }

    pub fn kind_name(&self) -> &'static str {
        if self.is_mapping() {
            "a mapping"
        } else if self.is_sequence() {
            "a sequence"
        } else {
            "a scalar"
        }
    }

    /// Require a mapping, or report `code`.
    pub fn expect_mapping(&self, diags: &mut Diagnostics, what: &str) -> Option<Mapping<'a>> {
        let mapping = self.as_mapping();
        if mapping.is_none() {
            diags.error(
                "yaml.shape",
                self.span(),
                format!("{what} must be a mapping, found {}", self.kind_name()),
            );
        }
        mapping
    }

    pub fn expect_sequence(&self, diags: &mut Diagnostics, what: &str) -> Option<Sequence<'a>> {
        let sequence = self.as_sequence();
        if sequence.is_none() {
            diags.error(
                "yaml.shape",
                self.span(),
                format!("{what} must be a sequence, found {}", self.kind_name()),
            );
        }
        sequence
    }

    pub fn expect_scalar(&self, diags: &mut Diagnostics, what: &str) -> Option<Scalar<'a>> {
        let scalar = self.as_scalar();
        if scalar.is_none() {
            diags.error(
                "yaml.shape",
                self.span(),
                format!("{what} must be a scalar, found {}", self.kind_name()),
            );
        }
        scalar
    }

    /// Convert to a JSON value, inferring scalar types the way YAML 1.2 core
    /// does for plain scalars and keeping quoted scalars as strings.
    pub fn to_json(&self) -> Value {
        if let Some(m) = self.as_mapping() {
            let mut out = serde_json::Map::new();
            for (key, value) in m.iter() {
                out.insert(key.to_string(), value.to_json());
            }
            Value::Object(out)
        } else if let Some(s) = self.as_sequence() {
            Value::Array(s.iter().map(|n| n.to_json()).collect())
        } else if let Some(s) = self.as_scalar() {
            s.to_json()
        } else {
            Value::Null
        }
    }
}

pub struct Mapping<'a> {
    file:  &'a str,
    inner: &'a MarkedMappingNode,
    span:  Span,
}

impl<'a> Mapping<'a> {
    pub fn span(&self) -> Span {
        self.span.clone()
    }

    pub fn get(&self, key: &str) -> Option<Node<'a>> {
        self.inner.get(key).map(|n| Node {
            file:  self.file,
            inner: n,
        })
    }

    /// Case-insensitive lookup, for formats whose keys are.
    pub fn get_ci(&self, key: &str) -> Option<Node<'a>> {
        let lowered = key.to_lowercase();
        self.inner
            .iter()
            .find(|(k, _)| k.as_str().to_lowercase() == lowered)
            .map(|(_, n)| Node {
                file:  self.file,
                inner: n,
            })
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.inner.contains_key(key)
    }

    /// Entries in document order.
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, Node<'a>)> + '_ {
        self.inner.iter().map(|(k, v)| {
            (k.as_str(), Node {
                file:  self.file,
                inner: v,
            })
        })
    }

    /// Keys with their own spans, for pointing at a bad key.
    pub fn keys(&self) -> impl Iterator<Item = (&'a str, Span)> + '_ {
        self.inner.iter().map(|(k, _)| {
            let start = k.span().start();
            (
                k.as_str(),
                Span::new(
                    self.file,
                    start.map_or(0, |m| m.line() as u32),
                    start.map_or(0, |m| m.column() as u32),
                ),
            )
        })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Report every key not in `allowed` as `code`. Formats reject unknown keys
    /// so a typo cannot be silently ignored.
    pub fn reject_unknown_keys(&self, allowed: &[&str], diags: &mut Diagnostics, where_: &str) {
        for (key, span) in self.keys() {
            if !allowed.contains(&key) {
                diags.error(
                    "yaml.unknown_key",
                    span,
                    format!("unknown key `{key}` in {where_}"),
                );
            }
        }
    }
}

pub struct Sequence<'a> {
    file:  &'a str,
    inner: &'a MarkedSequenceNode,
    span:  Span,
}

impl<'a> Sequence<'a> {
    pub fn span(&self) -> Span {
        self.span.clone()
    }

    pub fn iter(&self) -> impl Iterator<Item = Node<'a>> + '_ {
        self.inner.iter().map(|n| Node {
            file:  self.file,
            inner: n,
        })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

pub struct Scalar<'a> {
    inner: &'a MarkedScalarNode,
    span:  Span,
}

impl<'a> Scalar<'a> {
    pub fn span(&self) -> Span {
        self.span.clone()
    }

    pub fn as_str(&self) -> &'a str {
        self.inner.as_str()
    }

    /// Whether the scalar was written unquoted, so YAML type inference applies.
    pub fn is_plain(&self) -> bool {
        self.inner.may_coerce()
    }

    pub fn as_bool(&self) -> Option<bool> {
        if !self.is_plain() {
            return None;
        }
        match self.as_str() {
            "true" | "True" | "TRUE" => Some(true),
            "false" | "False" | "FALSE" => Some(false),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        if !self.is_plain() {
            return None;
        }
        self.as_str().parse().ok()
    }

    pub fn as_f64(&self) -> Option<f64> {
        if !self.is_plain() {
            return None;
        }
        let s = self.as_str();
        if s.parse::<i64>().is_ok() {
            return None;
        }
        s.parse().ok()
    }

    pub fn is_null(&self) -> bool {
        self.is_plain() && matches!(self.as_str(), "" | "~" | "null" | "Null" | "NULL")
    }

    pub fn to_json(&self) -> Value {
        if self.is_null() {
            Value::Null
        } else if let Some(b) = self.as_bool() {
            Value::Bool(b)
        } else if let Some(i) = self.as_i64() {
            Value::from(i)
        } else if let Some(f) = self.as_f64() {
            serde_json::Number::from_f64(f)
                .map_or_else(|| Value::String(self.as_str().to_string()), Value::Number)
        } else {
            Value::String(self.as_str().to_string())
        }
    }
}
