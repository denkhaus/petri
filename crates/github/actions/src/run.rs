//! `github/run`: a `run:` step with GitHub's runner contract.

use std::path::{Component, PathBuf};

use ir::{Outcome, Value};
use serde_json::Map;
use smol_str::SmolStr;
use steps::{Step, StepCtx, StepFailure, ValueOrSecretRef};

use crate::config::RunConfig;
use crate::gate;
use crate::session::{
    REPO_DIR, ResolvedProcess, Session, can_use_unsecure_commands, fold_into_outcome,
};

/// The process step, with the `GITHUB_*` files around it.
pub struct RunStep;

#[async_trait::async_trait]
impl Step for RunStep {
    const NAME: &'static str = frontend_gha::RUN_KIND;
    type Config = RunConfig;

    fn check_raw(&self, config: &Value) -> Result<(), StepFailure> {
        steps::check_misplaced_secret(config, &["env"])
    }

    #[tracing::instrument(
        name = "github.run_step",
        level = "debug",
        skip_all,
        fields(shell = ?config.shell, custom_shell = config.shell_command.is_some())
    )]
    async fn run(&self, config: RunConfig, ctx: StepCtx) -> Outcome {
        // The gate first: nothing is created and nothing spawns for a step whose
        // condition is false.
        match gate::refusal(
            config.gate.as_ref(),
            config.cancelled,
            &config.env,
            config.job_environment.as_deref(),
            config.background.as_deref(),
            &ctx,
        )
        .await
        {
            Ok(None) => {}
            Ok(Some(outcome)) => return outcome,
            Err(failure) => return failure.into(),
        }
        let session = match Session::begin(
            &ctx,
            &config.event,
            config.job_environment.as_deref(),
            config.background.as_deref(),
        )
        .await
        {
            Ok(session) => session,
            Err(failure) => return failure.into(),
        };
        let mut env = config.env;
        for (key, value) in session.env(&ctx.node) {
            env.entry(key)
                .or_insert_with(|| ValueOrSecretRef::Literal(Value::String(value.to_string())));
        }
        let allow_unsecure = can_use_unsecure_commands(&env, &*ctx.env);
        // `.` components drop out: `create_dir_all("repo/.")` cannot make
        // `repo` (its parent is the workspace, not `repo`), and
        // `working-directory: .` is how workflows spell the workspace itself.
        let working_dir: PathBuf = PathBuf::from(REPO_DIR)
            .join(config.working_dir.unwrap_or_default())
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        // The script always rides in a file. With a custom shell the prologue
        // joins the session's wrapper line; on the default path it stays part
        // of the script text. The session assembles both.
        let run = match &config.shell_command {
            Some(_) => config.run,
            None => format!("{}{}", session.prologue(), config.run),
        };
        let process = ResolvedProcess {
            run,
            shell: config.shell,
            env,
            working_dir: Some(working_dir),
            soft_fail: config.soft_fail,
            output_env_aliases: vec![SmolStr::new("GITHUB_OUTPUT")],
        };
        let (outcome, effects) = session
            .run(
                process,
                config.shell_command,
                config.shell_script,
                ctx,
                allow_unsecure,
            )
            .await;
        fold_into_outcome(outcome, effects, Map::new())
    }
}
