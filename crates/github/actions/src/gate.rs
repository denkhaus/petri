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

use frontend_gha::exprs::{
    has_hashfiles_sentinel, has_workspace_sentinel, replace_workspace_sentinels,
};
use frontend_gha::gate::{Gate, GateOp, eval};
use ir::expr::builtins::loose;
use ir::{Outcome, Value};
use smol_str::SmolStr;
use steps::{StepCtx, StepFailure, ValueOrSecretRef};

use crate::session::{github_workspace_path, read_job_env};

/// The step's gate could not be read or evaluated.
pub const GATE_CLASS: &str = "gate";

/// Evaluate a step's gate and decide what a refused step records instead of
/// running: `None` means run; a false gate is `Cancelled` when the scope was
/// cancelled at firing — as GitHub reports post-cancel non-cleanup steps —
/// else `Skipped`. No gate means run.
pub(crate) async fn refusal(
    gate: Option<&Value>,
    cancelled: bool,
    env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
    ctx: &StepCtx,
) -> Result<Option<Outcome>, StepFailure> {
    let Some(gate) = gate else { return Ok(None) };
    if admitted(gate, env_config, ctx).await? {
        return Ok(None);
    }
    Ok(Some(if cancelled {
        Outcome::cancelled()
    } else {
        Outcome::skipped()
    }))
}

/// Evaluate a step's gate: `true` means run. `env_config` is the step's own
/// `env:` from its config, which wins over the job's accumulated `GITHUB_ENV`.
async fn admitted(
    gate: &Value,
    env_config: &BTreeMap<SmolStr, ValueOrSecretRef>,
    ctx: &StepCtx,
) -> Result<bool, StepFailure> {
    let mut gate = Gate::from_value(gate).map_err(|message| StepFailure {
        class: GATE_CLASS,
        message: format!("the step's gate is not one this runner wrote: {message}"),
    })?;

    // The engine-resolved prefix can refuse on its own: the prereq terms (job
    // started, the implicit `success()`) ride as leading `&&` literals, and
    // evaluation would short-circuit on a false one before reaching anything
    // step-resolved — so a step that will not run anyway skips the workspace
    // hash and the env-file read.
    if refused_before_resolution(&gate) {
        return Ok(false);
    }

    resolve_hashfiles(&mut gate, ctx).await?;

    // The job env file is control input the gate may not even need.
    let job_env = if gate.reads_env() {
        read_job_env(&*ctx.env).await?
    } else {
        BTreeMap::new()
    };

    let value = eval(&gate, &mut |name| {
        env_value(name, env_config, &job_env, ctx)
    })?;
    Ok(loose::truthy(&value))
}

/// Whether the gate's literal prefix — the root, or the leading operands of a
/// root `&&` up to the first step-resolved leaf — already lands on a falsy
/// value. Exactly what evaluation would short-circuit on without resolving
/// anything: an unresolved sentinel string is non-empty, so never falsy.
fn refused_before_resolution(gate: &Gate) -> bool {
    match gate {
        Gate::Lit(v) => !loose::truthy(v),
        Gate::Op {
            op: GateOp::And,
            args,
        } => args
            .iter()
            .map_while(|arg| match arg {
                Gate::Lit(v) => Some(v),
                _ => None,
            })
            .any(|v| !loose::truthy(v)),
        _ => false,
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
    let workspace = github_workspace_path(&*ctx.env);
    // `github.workspace` first: a literal this environment can answer outright.
    gate.map_texts(&mut |text| {
        has_workspace_sentinel(text).then(|| replace_workspace_sentinels(text, &workspace))
    });
    let calls =
        crate::hashfiles::resolved_calls(gate.texts().into_iter(), &*ctx.env, &workspace).await?;
    if calls.is_empty() {
        return Ok(());
    }
    gate.map_texts(&mut |text| {
        has_hashfiles_sentinel(text).then(|| crate::hashfiles::splice(text, &calls))
    });
    Ok(())
}
