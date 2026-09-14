//! Fabro's settings interpolation: `{{ inputs.NAME }}`, `{{ vars.NAME }}` and
//! `{{ goal }}` substitute at load; `{{ secrets.NAME }}` names a secret the
//! run resolves at spawn; `{{ env.NAME }}` parses but never resolves. An
//! unknown token body stays literal, as in Fabro.

use frontend_attractor::template::Context;

/// What a settings string interpolates to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interpolated {
    pub(crate) text:   String,
    /// The secret the whole value names, when it is exactly one
    /// `{{ secrets.NAME }}` token.
    pub(crate) secret: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InterpolationError {
    /// A `{{ secrets.NAME }}` token where none may appear, or one mixed with
    /// other text.
    SecretNotAllowed { name: String },
    /// An `{{ inputs.* }}` or `{{ vars.* }}` token no input binds.
    Unbound { name: String },
    /// `{{ env.NAME }}`.
    Env { name: String },
}

fn is_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Interpolate `text`. With `secret_value` the whole text may be one secret
/// token, which comes back as `secret`; otherwise a secret token is an error.
pub(crate) fn interpolate(
    text: &str,
    context: &Context,
    secret_value: bool,
) -> Result<Interpolated, InterpolationError> {
    let trimmed = text.trim();
    if secret_value
        && let Some(body) = trimmed
            .strip_prefix("{{")
            .and_then(|rest| rest.strip_suffix("}}"))
        && let Some(name) = body.trim().strip_prefix("secrets.")
        && is_name(name)
    {
        return Ok(Interpolated {
            text:   String::new(),
            secret: Some(name.to_string()),
        });
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else {
            out.push_str(rest);
            return Ok(Interpolated {
                text:   out,
                secret: None,
            });
        };
        let token = rest[start + 2..start + end].trim();
        out.push_str(&rest[..start]);
        if let Some(name) = token.strip_prefix("secrets.")
            && is_name(name)
        {
            return Err(InterpolationError::SecretNotAllowed {
                name: name.to_string(),
            });
        }
        if let Some(name) = token.strip_prefix("env.")
            && is_name(name)
        {
            return Err(InterpolationError::Env {
                name: name.to_string(),
            });
        }
        let bound = token
            .strip_prefix("inputs.")
            .or_else(|| token.strip_prefix("vars."))
            .is_some_and(is_name)
            || token == "goal";
        if bound {
            match context.token_text(token) {
                Ok(value) => out.push_str(&value),
                Err(_) => {
                    return Err(InterpolationError::Unbound {
                        name: token.to_string(),
                    });
                }
            }
        } else {
            out.push_str(&rest[start..start + end + 2]);
        }
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    Ok(Interpolated {
        text:   out,
        secret: None,
    })
}

#[cfg(test)]
mod tests {
    use frontend::CompileInputs;

    use super::*;

    fn context() -> Context {
        let inputs = CompileInputs::new().with_input("target", "main");
        let mut context = Context::new(&inputs);
        context.set_goal("Fix it".into());
        context
    }

    #[test]
    fn inputs_and_goal_substitute_and_unknown_tokens_stay() {
        let out = interpolate(
            "on {{ inputs.target }} for {{ goal }} {{ other.x }}",
            &context(),
            false,
        )
        .expect("interpolates");
        assert_eq!(out.text, "on main for Fix it {{ other.x }}");
        assert_eq!(out.secret, None);
    }

    #[test]
    fn a_whole_secret_token_names_the_secret_and_a_mixed_one_is_refused() {
        let out = interpolate(" {{ secrets.TOKEN }} ", &context(), true).expect("interpolates");
        assert_eq!(out.secret.as_deref(), Some("TOKEN"));
        assert_eq!(
            interpolate("x{{ secrets.TOKEN }}", &context(), true),
            Err(InterpolationError::SecretNotAllowed {
                name: "TOKEN".into(),
            })
        );
        assert_eq!(
            interpolate("{{ secrets.TOKEN }}", &context(), false),
            Err(InterpolationError::SecretNotAllowed {
                name: "TOKEN".into(),
            })
        );
    }

    #[test]
    fn env_tokens_and_unbound_inputs_are_errors() {
        assert_eq!(
            interpolate("{{ env.HOME }}", &context(), false),
            Err(InterpolationError::Env {
                name: "HOME".into(),
            })
        );
        assert_eq!(
            interpolate("{{ inputs.missing }}", &context(), false),
            Err(InterpolationError::Unbound {
                name: "inputs.missing".into(),
            })
        );
    }
}
