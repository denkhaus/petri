//! The `model_stylesheet` grammar: CSS-like rules that write model properties
//! onto nodes by `*`, shape, `.class` or `#id`, specificity 0 to 3. Explicit
//! node attributes always win.

use std::collections::BTreeMap;

use frontend::{Diagnostics, Span};

use crate::lower::shape_of;
use crate::model::{AttrValue, Workflow};

/// The properties a stylesheet may set.
pub const PROPERTIES: &[&str] = &["model", "provider", "reasoning_effort", "speed", "backend"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Universal,
    Shape(String),
    Class(String),
    Id(String),
}

impl Selector {
    pub const fn specificity(&self) -> u8 {
        match self {
            Self::Universal => 0,
            Self::Shape(_) => 1,
            Self::Class(_) => 2,
            Self::Id(_) => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub selector:     Selector,
    pub declarations: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StylesheetError(String);

/// Parse a stylesheet. Comments are `/* */`.
pub fn parse(input: &str) -> Result<Stylesheet, StylesheetError> {
    let input = strip_comments(input)?;
    let mut rest = input.trim();
    let mut rules = Vec::new();
    while !rest.is_empty() {
        let selector = selector(&mut rest)?;
        let Some(after) = rest.strip_prefix('{') else {
            return Err(StylesheetError(format!(
                "expected `{{` after the selector, found {:?}",
                excerpt(rest)
            )));
        };
        rest = after.trim_start();
        let declarations = declarations(&mut rest)?;
        rest = rest[1..].trim_start();
        rules.push(Rule {
            selector,
            declarations,
        });
    }
    Ok(Stylesheet { rules })
}

fn strip_comments(input: &str) -> Result<String, StylesheetError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("/*") {
        let body = &rest[start + 2..];
        let Some(end) = body.find("*/") else {
            return Err(StylesheetError(format!(
                "unterminated comment: {:?}",
                excerpt(&rest[start..])
            )));
        };
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = &body[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn excerpt(input: &str) -> String {
    input.chars().take(20).collect()
}

fn ident_end(text: &str, extra: impl Fn(char) -> bool) -> usize {
    text.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || extra(c)))
        .unwrap_or(text.len())
}

fn selector(rest: &mut &str) -> Result<Selector, StylesheetError> {
    if let Some(after) = rest.strip_prefix('*') {
        *rest = after.trim_start();
        return Ok(Selector::Universal);
    }
    if let Some(after) = rest.strip_prefix('#') {
        let end = ident_end(after, |_| false);
        if end == 0 {
            return Err(StylesheetError("expected a node id after `#`".into()));
        }
        *rest = after[end..].trim_start();
        return Ok(Selector::Id(after[..end].to_string()));
    }
    if let Some(after) = rest.strip_prefix('.') {
        let end = ident_end(after, |_| false);
        if end == 0 {
            return Err(StylesheetError("expected a class name after `.`".into()));
        }
        *rest = after[end..].trim_start();
        return Ok(Selector::Class(after[..end].to_string()));
    }
    let end = ident_end(rest, |_| false);
    if end == 0 {
        return Err(StylesheetError(format!(
            "expected a selector (`*`, `#id`, `.class` or a shape), found {:?}",
            excerpt(rest)
        )));
    }
    let shape = rest[..end].to_string();
    *rest = rest[end..].trim_start();
    Ok(Selector::Shape(shape))
}

fn declarations(rest: &mut &str) -> Result<Vec<(String, String)>, StylesheetError> {
    let mut out = Vec::new();
    loop {
        if rest.starts_with('}') {
            return Ok(out);
        }
        if rest.is_empty() {
            return Err(StylesheetError("expected `}`".into()));
        }
        if let Some(after) = rest.strip_prefix(';') {
            *rest = after.trim_start();
            continue;
        }
        let end = rest
            .find(|c: char| c == ':' || c.is_whitespace())
            .unwrap_or(rest.len());
        let property = rest[..end].to_string();
        *rest = rest[end..].trim_start();
        let Some(after) = rest.strip_prefix(':') else {
            return Err(StylesheetError(format!(
                "expected `:` after the property `{property}`"
            )));
        };
        *rest = after.trim_start();
        let end = rest.find([';', '}']).unwrap_or(rest.len());
        let value = rest[..end].trim().to_string();
        *rest = rest[end..].trim_start();
        if value.is_empty() {
            return Err(StylesheetError(format!(
                "the property `{property}` has no value"
            )));
        }
        out.push((property, value));
        if let Some(after) = rest.strip_prefix(';') {
            *rest = after.trim_start();
        }
    }
}

/// Write the stylesheet's properties onto the nodes: higher specificity wins,
/// later rules win ties, and an attribute the node sets itself is never
/// touched. Unknown properties are diagnosed once each.
pub fn apply(
    stylesheet: &Stylesheet,
    workflow: &mut Workflow,
    span: &Span,
    diags: &mut Diagnostics,
) {
    let mut rules: Vec<&Rule> = stylesheet.rules.iter().collect();
    rules.sort_by_key(|r| r.selector.specificity());
    let mut unknown = Vec::new();
    for rule in &rules {
        for (property, _) in &rule.declarations {
            if !PROPERTIES.contains(&property.as_str()) && !unknown.contains(property) {
                unknown.push(property.clone());
            }
        }
    }
    for property in unknown {
        diags.warning(
            "fabro.stylesheet.unknown_property",
            span.clone(),
            format!(
                "the stylesheet sets `{property}`, which is not a model property ({}); it is ignored",
                PROPERTIES.join(", ")
            ),
        );
    }
    for node in &mut workflow.nodes {
        let shape = shape_of(node);
        let mut applied: BTreeMap<&str, (&str, u8)> = BTreeMap::new();
        for rule in &rules {
            let matches = match &rule.selector {
                Selector::Universal => true,
                Selector::Shape(s) => *s == shape,
                Selector::Class(c) => node.classes.iter().any(|k| k == c),
                Selector::Id(id) => *id == node.id,
            };
            if !matches {
                continue;
            }
            let specificity = rule.selector.specificity();
            for (property, value) in &rule.declarations {
                if !PROPERTIES.contains(&property.as_str()) {
                    continue;
                }
                match applied.get(property.as_str()) {
                    Some((_, existing)) if specificity < *existing => {}
                    _ => {
                        applied.insert(property, (value, specificity));
                    }
                }
            }
        }
        for (property, (value, _)) in applied {
            if !node.attrs.contains(property) {
                node.attrs
                    .insert(property, AttrValue::Str(value.to_string()), span.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_with_comments_and_specificity() {
        let sheet = parse(
            "/* defaults */ * { model: a; reasoning_effort: low; }\n.code { model: b }\n#x { provider: p; }\nbox { speed: fast }",
        )
        .expect("parses");
        assert_eq!(sheet.rules.len(), 4);
        assert_eq!(sheet.rules[0].selector, Selector::Universal);
        assert_eq!(sheet.rules[1].selector, Selector::Class("code".into()));
        assert_eq!(sheet.rules[2].selector, Selector::Id("x".into()));
        assert_eq!(sheet.rules[3].selector, Selector::Shape("box".into()));
        assert_eq!(sheet.rules[0].declarations.len(), 2);
    }

    #[test]
    fn rejects_malformed_sheets() {
        assert!(parse("{ model: a }").is_err());
        assert!(parse("* model: a }").is_err());
        assert!(parse("* { model: }").is_err());
        assert!(parse("/* open").is_err());
    }
}
