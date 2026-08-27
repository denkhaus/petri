//! Evaluating a step's gate at spawn.
//!
//! The frontend lowers every step-level condition into the step's config as a
//! gate tree ([`frontend_gha::gate`]); by the time a step deserializes it, the
//! engine has resolved every `{"$expr": id}` leaf to a literal. What remains is
//! the step's half:
//!
//! - `hashFiles` sentinels in string literals resolve against the workspace, in
//!   the job environment, exactly as they do in a `run:` script;
//! - `{"$env": NAME}` leaves resolve from the environment the process would get —
//!   the step's own `env:` config first (secrets included, fetched from the run's
//!   provider and never written down), then the job's accumulated `GITHUB_ENV` —
//!   falling back to the leaf's engine-resolved `or` (the scope env);
//! - the operators evaluate with GitHub's loose semantics, shared with the
//!   engine through `ir`'s loose primitives, and GitHub truthiness lands at the
//!   root.
//!
//! All of it happens before session files are created and before any process
//! spawns: a false gate leaves nothing behind but the step's status.

use std::collections::BTreeMap;

use frontend_gha::exprs::{has_hashfiles_sentinel, hashfiles_calls, replace_hashfiles_sentinels};
use frontend_gha::gate::{Gate, eval};
use ir::Value;
use ir::expr::builtins::loose;
use smol_str::SmolStr;
use steps::{StepCtx, StepFailure, ValueOrSecretRef};

use crate::session::{github_workspace_path, read_job_env};

/// The step's gate could not be read or evaluated.
pub const GATE_CLASS: &str = "gate";

/// Evaluate a step's gate: `true` means run. `env_config` is the step's own
/// `env:` from its config, which wins over the job's accumulated `GITHUB_ENV`.
pub(crate) async fn admitted(
    gate: &Value,
    env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
    ctx: &StepCtx,
) -> Result<bool, StepFailure> {
    let mut gate = Gate::from_value(gate).map_err(|message| StepFailure {
        class: GATE_CLASS,
        message: format!("the step's gate is not one this runner wrote: {message}"),
    })?;

    resolve_hashfiles(&mut gate, ctx).await?;

    // The job env file is control input the gate may not even need.
    let job_env = if gate.reads_env() {
        read_job_env(&*ctx.env).await?
    } else {
        BTreeMap::new()
    };

    let mut failure: Option<StepFailure> = None;
    let mut lookup = |name: &str| -> Result<Option<Value>, ()> {
        match env_value(name, env_config, &job_env, ctx) {
            Ok(found) => Ok(found),
            Err(e) => {
                failure = Some(e);
                Err(())
            }
        }
    };
    let value = eval(&gate, &mut lookup);
    match value {
        Ok(value) => Ok(loose::truthy(&value)),
        Err(()) => Err(failure.expect("the lookup that failed recorded why")),
    }
}

/// One env leaf's value: the step's own `env:` first (a secret reference is
/// fetched from the run's provider, which registers it for masking), then the
/// job's accumulated `GITHUB_ENV`. Lookups are case-insensitive on a miss, as
/// the `env` context's are.
fn env_value(
    name: &str,
    env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
    job_env: &BTreeMap<String, String>,
    ctx: &StepCtx,
) -> Result<Option<Value>, StepFailure> {
    let configured = env_config
        .get(name)
        .or_else(|| ci_find(env_config.iter().map(|(k, v)| (k.as_str(), v)), name));
    if let Some(value) = configured {
        return Ok(Some(match value {
            ValueOrSecretRef::Secret { name } => {
                let secret = ctx.secrets.resolve(name).map_err(|e| StepFailure {
                    class: steps::SECRET_UNAVAILABLE_CLASS,
                    message: e.to_string(),
                })?;
                Value::String(secret.expose().to_string())
            }
            ValueOrSecretRef::Literal(v) => Value::String(crate::session::stringify(v)),
        }));
    }
    let accumulated = job_env
        .get(name)
        .or_else(|| ci_find(job_env.iter().map(|(k, v)| (k.as_str(), v)), name));
    Ok(accumulated.map(|v| Value::String(v.clone())))
}

fn ci_find<'a, T>(mut entries: impl Iterator<Item = (&'a str, T)>, name: &str) -> Option<T> {
    let lowered = name.to_lowercase();
    entries
        .find(|(k, _)| k.to_lowercase() == lowered)
        .map(|(_, v)| v)
}

/// Resolve every `hashFiles` sentinel in the gate's string literals, in one
/// workspace walk, before evaluation compares anything.
async fn resolve_hashfiles(gate: &mut Gate, ctx: &StepCtx) -> Result<(), StepFailure> {
    let mut calls: BTreeMap<Vec<String>, String> = BTreeMap::new();
    for text in gate.texts() {
        for patterns in hashfiles_calls(text) {
            calls.entry(patterns).or_default();
        }
    }
    if calls.is_empty() {
        return Ok(());
    }
    let patterns: Vec<Vec<String>> = calls.keys().cloned().collect();
    let workspace = github_workspace_path(&*ctx.env);
    let hashes = crate::hashfiles::compute(&*ctx.env, &workspace, &patterns).await?;
    for (patterns, hash) in patterns.into_iter().zip(hashes) {
        calls.insert(patterns, hash);
    }
    gate.map_texts(&mut |text| {
        if !has_hashfiles_sentinel(text) {
            return None;
        }
        let resolved = replace_hashfiles_sentinels(
            text,
            |patterns| -> Result<String, std::convert::Infallible> {
                Ok(calls.get(patterns).cloned().unwrap_or_default())
            },
        )
        .expect("the resolver is infallible");
        Some(resolved)
    });
    Ok(())
}
