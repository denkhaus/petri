//! Edge-label normalization, shared by the lowering and the step kinds.
//!
//! Fabro's preferred-label routing compares labels after stripping an
//! accelerator prefix (`[Y] Yes`, `Y) Yes`, `Y - Yes` all read `Yes`),
//! trimming and lowercasing. The engine's `normalize_label` builtin
//! lowercases and collapses punctuation but knows nothing about
//! accelerators, so the two halves split the work: a step strips the
//! accelerator from the label it reports ([`strip_accelerator`]), and the
//! lowering strips it from the static edge label before handing the rest to
//! the builtin. Both sides then meet on the builtin's spelling.

/// The label without its accelerator prefix, trimmed. The three prefix shapes
/// are Fabro's: `[K] label`, `K) label`, `K - label`.
pub fn strip_accelerator(label: &str) -> &str {
    let trimmed = label.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return rest[end + 1..].trim_start();
    }
    if trimmed.len() >= 2 && trimmed.as_bytes()[1] == b')' {
        return trimmed[2..].trim_start();
    }
    if trimmed.len() >= 3 && trimmed.is_char_boundary(1) && trimmed[1..].starts_with(" - ") {
        return &trimmed[4..];
    }
    trimmed
}

/// The accelerator key a label carries, or its first character: what a human
/// gate presents as the shortcut for the choice.
pub fn accelerator_key(label: &str) -> String {
    let trimmed = label.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(end) = rest.find(']')
        && end > 0
    {
        return rest[..end].to_string();
    }
    if let Some(pos) = trimmed.find(')')
        && (1..=3).contains(&pos)
        && trimmed[..pos].chars().all(char::is_alphanumeric)
    {
        return trimmed[..pos].to_string();
    }
    if let Some(pos) = trimmed.find(" - ")
        && (1..=3).contains(&pos)
        && trimmed[..pos].chars().all(char::is_alphanumeric)
    {
        return trimmed[..pos].to_string();
    }
    trimmed
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// The engine's `normalize_label` builtin, evaluated on `text`: the one
/// spelling both sides of a preferred-label guard agree on.
pub fn engine_normalized(text: &str) -> String {
    let mut table = ir::ExprTable::new();
    let lit = table.lit(text);
    let call = table.call("normalize_label", vec![lit]);
    let run = ir::RunContext::new();
    let statics = ir::StaticCtx::new();
    let env = ir::EvalEnv::new(&serde_json::Value::Null, &run, &statics);
    match ir::eval(&table, call, &env) {
        Ok(serde_json::Value::String(s)) => s,
        _ => text.to_lowercase(),
    }
}

/// The static key a preferred-label guard compares against: the accelerator
/// stripped, then the engine's normalization.
pub fn routing_key(label: &str) -> String {
    engine_normalized(strip_accelerator(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_three_accelerator_shapes() {
        assert_eq!(strip_accelerator("[A] Approve"), "Approve");
        assert_eq!(strip_accelerator("Y) Yes"), "Yes");
        assert_eq!(strip_accelerator("Y - Yes"), "Yes");
        assert_eq!(strip_accelerator("  next  "), "next");
        assert_eq!(strip_accelerator("No"), "No");
    }

    #[test]
    fn accelerator_keys_follow_fabro() {
        assert_eq!(accelerator_key("[A] Approve"), "A");
        assert_eq!(accelerator_key("Y) Yes"), "Y");
        assert_eq!(accelerator_key("Y - Yes"), "Y");
        assert_eq!(accelerator_key("Approve"), "A");
    }

    #[test]
    fn routing_keys_meet_on_the_engine_spelling() {
        assert_eq!(routing_key("[N] No clear verdict"), "no_clear_verdict");
        assert_eq!(engine_normalized("No clear verdict"), "no_clear_verdict");
    }
}
