//! Evaluating a step's gate at spawn.
//!
//! The frontend lowers every step-level condition into the step's config as a
//! gate tree ([`frontend_gha::gate`]); by the time a step deserializes it, the
//! engine has resolved every `{"$expr": id}` leaf to a literal. What remains is
//! the step's half:
//!
//! - `hashFiles` sentinels in string literals resolve against the workspace, in
//!   the job environment, exactly as they do in a `run:` script;
//! - `{"$env": NAME}` leaves resolve from the environment the process would get
//!   — the step's own `env:` config first (secrets included, fetched from the
//!   run's provider and never written down), then the job's accumulated
//!   `GITHUB_ENV` — falling back to the leaf's engine-resolved `or` (the scope
//!   env);
//! - the operators evaluate with GitHub's loose semantics, shared with the
//!   engine through `ir`'s loose primitives, and GitHub truthiness lands at the
//!   root.
//!
//! All of it happens before session files are created and before any process
//! spawns: a false gate leaves nothing behind but the step's status.

use std::collections::BTreeMap;

use frontend_gha::exprs::{
    has_env_sentinel, has_hashfiles_sentinel, has_runner_temp_sentinel,
    has_runner_tool_cache_sentinel, has_workspace_sentinel, replace_env_sentinels,
    replace_runner_temp_sentinels, replace_runner_tool_cache_sentinels,
    replace_workspace_sentinels,
};
use frontend_gha::gate::{Gate, GateOp, eval};
use ir::expr::builtins::loose;
use ir::{Outcome, Value};
use smol_str::SmolStr;
use steps::{StepCtx, StepFailure, ValueOrSecretRef};

use crate::hashfiles;
use crate::session::{
    ci_get, env_tool_cache, github_workspace_path, read_job_env, runner_temp_path, stringify,
};

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
    let mut gate = Gate::try_from(gate).map_err(|message| StepFailure {
        class:   GATE_CLASS,
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

    // The job env file is control input the gate may not even need — but a
    // `runner.tool_cache` leaf does, resolution's second rung being a mid-job
    // `GITHUB_ENV` export, and so does an env sentinel in a string literal.
    let wants_tool_cache = gate.texts().into_iter().any(has_runner_tool_cache_sentinel);
    let wants_env = gate.texts().into_iter().any(has_env_sentinel);
    let job_env = if gate.reads_env() || wants_tool_cache || wants_env {
        read_job_env(&*ctx.env).await?
    } else {
        BTreeMap::new()
    };
    if wants_tool_cache {
        let store = ctx.capability::<crate::ToolCacheCap>();
        let tool_cache = env_tool_cache(
            &*ctx.env,
            store.as_ref().map(|cap| cap.0.as_path()),
            env_config,
            &job_env,
        );
        gate.map_texts(&mut |text| {
            has_runner_tool_cache_sentinel(text)
                .then(|| replace_runner_tool_cache_sentinels(text, &tool_cache))
        });
    }

    // An env sentinel in a gate literal — a caller's `with:` value woven into
    // a composite condition — resolves through the same rungs an env leaf
    // uses, the ambient env answering last, as the spawn substitution does.
    if wants_env {
        let mut failed = None;
        gate.map_texts(&mut |text| {
            if !has_env_sentinel(text) || failed.is_some() {
                return None;
            }
            match replace_env_sentinels(text, |name| {
                env_value(name, env_config, &job_env, ctx).map(|value| match value {
                    Some(Value::String(s)) => s,
                    Some(other) => stringify(&other),
                    None => ctx.env.ambient_env(name).unwrap_or_default(),
                })
            }) {
                Ok(resolved) => Some(resolved),
                Err(failure) => {
                    failed = Some(failure);
                    None
                }
            }
        });
        if let Some(failure) = failed {
            return Err(failure);
        }
    }

    let value = eval(&gate, &mut |name| {
        env_value(name, env_config, &job_env, ctx)
    })?;
    Ok(loose::is_truthy(&value))
}

/// Whether the gate's literal prefix — the root, or the leading operands of a
/// root `&&` up to the first step-resolved leaf — already lands on a falsy
/// value. Exactly what evaluation would short-circuit on without resolving
/// anything: an unresolved sentinel string is non-empty, so never falsy.
fn refused_before_resolution(gate: &Gate) -> bool {
    match gate {
        Gate::Lit(v) => !loose::is_truthy(v),
        Gate::Op {
            op: GateOp::And,
            args,
        } => args
            .iter()
            .map_while(|arg| match arg {
                Gate::Lit(v) => Some(v),
                _ => None,
            })
            .any(|v| !loose::is_truthy(v)),
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
    if let Some(value) = ci_get(env_config, name) {
        return Ok(Some(match value {
            ValueOrSecretRef::Secret { name } => {
                let secret = ctx.secrets.resolve(name).map_err(|e| StepFailure {
                    class:   steps::SECRET_UNAVAILABLE_CLASS,
                    message: e.to_string(),
                })?;
                Value::String(secret.expose().to_string())
            }
            ValueOrSecretRef::Literal(v) => Value::String(stringify(v)),
        }));
    }
    Ok(ci_get(job_env, name).map(|v| Value::String(v.clone())))
}

/// Resolve every `hashFiles` sentinel in the gate's string literals, in one
/// workspace walk, before evaluation compares anything.
async fn resolve_hashfiles(gate: &mut Gate, ctx: &StepCtx) -> Result<(), StepFailure> {
    let workspace = github_workspace_path(&*ctx.env);
    // `github.workspace` and `runner.temp` first: literals this environment
    // can answer outright.
    gate.map_texts(&mut |text| {
        has_workspace_sentinel(text).then(|| replace_workspace_sentinels(text, &workspace))
    });
    let runner_temp = runner_temp_path(ctx.env.workspace_path());
    gate.map_texts(&mut |text| {
        has_runner_temp_sentinel(text).then(|| replace_runner_temp_sentinels(text, &runner_temp))
    });
    let calls = hashfiles::resolved_calls(gate.texts().into_iter(), &*ctx.env, &workspace).await?;
    if calls.is_empty() {
        return Ok(());
    }
    gate.map_texts(&mut |text| {
        has_hashfiles_sentinel(text).then(|| hashfiles::splice(text, &calls))
    });
    Ok(())
}
