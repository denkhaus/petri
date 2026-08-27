//! `github/run`: a `run:` step with GitHub's runner contract.

use std::path::PathBuf;

use ir::{Outcome, Value};
use serde_json::Map;
use smol_str::SmolStr;
use steps::{ProcessConfig, Step, StepCtx, StepFailure, ValueOrSecretRef};

use crate::config::RunConfig;
use crate::session::{REPO_DIR, Session, env_truthy, fold_into_outcome};

/// The process step, with the `GITHUB_*` files around it.
pub struct RunStep;

#[async_trait::async_trait]
impl Step for RunStep {
    const NAME: &'static str = frontend_gha::RUN_KIND;
    type Config = RunConfig;

    fn check_raw(&self, config: &Value) -> Result<(), StepFailure> {
        steps::check_misplaced_secret(config, &["env"])
    }

    async fn run(&self, config: RunConfig, ctx: StepCtx) -> Outcome {
        // The gate first: nothing is created and nothing spawns for a step whose
        // condition is false.
        match crate::gate::refusal(config.gate.as_ref(), config.cancelled, &config.env, &ctx).await
        {
            Ok(None) => {}
            Ok(Some(outcome)) => return outcome,
            Err(failure) => return failure.into(),
        }
        let session = match Session::begin(&ctx, &config.event).await {
            Ok(session) => session,
            Err(failure) => return failure.into(),
        };
        let mut env = config.env;
        for (key, value) in session.env(&ctx.node) {
            env.entry(key)
                .or_insert_with(|| ValueOrSecretRef::Literal(Value::String(value.to_string())));
        }
        let allow_unsecure = env_truthy(&env, "ACTIONS_ALLOW_UNSECURE_COMMANDS");
        let working_dir = match config.working_dir {
            Some(dir) => PathBuf::from(REPO_DIR).join(dir),
            None => PathBuf::from(REPO_DIR),
        };
        // With a custom shell the script rides in a file and the prologue joins
        // the wrapper line instead; the session assembles both.
        let run = match &config.shell_command {
            Some(_) => config.run,
            None => format!("{}{}", session.prologue(), config.run),
        };
        let process = ProcessConfig {
            run,
            shell: config.shell,
            env,
            working_dir: Some(working_dir),
            soft_fail: config.soft_fail,
            output_env_aliases: vec![SmolStr::new("GITHUB_OUTPUT")],
        };
        let (outcome, effects) = session
            .run(process, config.shell_command, ctx, allow_unsecure)
            .await;
        fold_into_outcome(outcome, effects, Map::new())
    }
}
