//! `github/docker_action`: one phase of a Docker container action.
//!
//! GitHub's container-action contract: one container per phase invocation, the
//! job workspace mounted, declared inputs as `INPUT_*`, the manifest's `args`
//! (expression-interpolated) to the entrypoint, and the exit code as the
//! outcome. The step assembles all of that and hands it to the scope-bound
//! [`ContainerRunner`] — it never learns which daemon runs it, and it never
//! goes through `ExecEnv::spawn` (that would be Docker-in-Docker for a
//! containerized job).

use std::collections::BTreeMap;
use std::path::PathBuf;

use executor::{ContainerImage, OneShotContainer};
use frontend_gha::action::validate_relative_action_path;
use frontend_gha::exprs::{
    has_hashfiles_sentinel, has_secret_sentinel, has_sentinel_escape, replace_secret_sentinels,
    unescape_sentinel_text,
};
use ir::{Outcome, StepEvent, Value};
use serde_json::Map;
use smol_str::SmolStr;
use steps::{
    Ending, Step, StepCtx, StepFailure, ValueOrSecretRef, ladder, natural_outcome,
    parse_outputs,
};
use tokio::sync::mpsc;

use crate::action::{ActionSourceCap, stage};
use crate::commands::CommandSink;
use crate::config::{DockerActionConfig, DockerActionImage, DockerfileImage};
use crate::session::{
    REPO_DIR, SINK_LIMIT, Session, fold_into_outcome, github_workspace_path, stringify,
};

/// The action's image could not be prepared.
pub const IMAGE_CLASS: &str = "action_image";

/// Where the once-per-run build markers live, relative to the workspace: a
/// local action's Dockerfile builds fresh once per scope, then its tag is
/// reused by the later phases and steps of the same job.
const BUILD_MARKER_DIR: &str = ".ci/docker-built";

/// Runs one phase of a Docker container action through the scope's runner.
pub struct DockerActionStep;

#[async_trait::async_trait]
impl Step for DockerActionStep {
    const NAME: &'static str = frontend_gha::DOCKER_ACTION_KIND;
    type Config = DockerActionConfig;

    fn check_raw(&self, config: &Value) -> Result<(), StepFailure> {
        steps::check_misplaced_secret(config, &["env"])
    }

    async fn run(&self, config: DockerActionConfig, ctx: StepCtx) -> Outcome {
        match execute(config, ctx).await {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }
}

async fn execute(config: DockerActionConfig, mut ctx: StepCtx) -> Result<Outcome, StepFailure> {
    // The gate first: a phase whose condition is false pulls nothing and
    // creates nothing.
    if let Some(outcome) =
        crate::gate::refusal(config.gate.as_ref(), config.cancelled, &config.env, &ctx).await?
    {
        return Ok(outcome);
    }
    let runner = ctx.require_container_runner()?;
    let mut session = Session::begin(&ctx, &config.event).await?;

    let (image, repository, git_ref) = prepare_image(&config.image, &ctx).await?;

    // Everything textual resolves before the container exists: hashFiles
    // against the workspace, then secret sentinels from the run's provider.
    let resolver = TextResolver::begin(&config, &ctx).await?;
    let resolve = |value: &Value| resolver.resolve(&stringify(value), &ctx);

    let mut env: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
    for (key, value) in &config.env {
        let resolved = match value {
            ValueOrSecretRef::Secret { name } => ctx
                .secrets
                .resolve(name)
                .map_err(|e| StepFailure {
                    class: steps::SECRET_UNAVAILABLE_CLASS,
                    message: e.to_string(),
                })?
                .expose()
                .to_string(),
            ValueOrSecretRef::Literal(literal) => resolve(literal)?,
        };
        env.insert(key.clone(), SmolStr::new(resolved));
    }
    for (name, value) in &config.inputs {
        env.insert(
            SmolStr::new(crate::action::input_variable(name)),
            SmolStr::new(resolve(value)?),
        );
    }
    for (name, value) in &config.state {
        env.insert(
            SmolStr::new(format!("STATE_{name}")),
            SmolStr::new(stringify(value)),
        );
    }
    // The runner contract's files and directories, at their paths *inside the
    // action container*: the workspace rides at the runner's mount point,
    // wherever the job environment itself has it.
    let root = runner.workspace_path().to_string();
    for (key, value) in session.env_rooted(&ctx.node, &root) {
        env.entry(key).or_insert(value);
    }
    env.insert(
        SmolStr::new("GITHUB_ACTION_REPOSITORY"),
        SmolStr::new(repository),
    );
    env.insert(SmolStr::new("GITHUB_ACTION_REF"), SmolStr::new(git_ref));
    // The outputs file, created empty so reading it back never depends on the
    // action having written one.
    let output_rel = format!(".ci/out/{}.env", ctx.firing.raw());
    let output_rel_path = PathBuf::from(&output_rel);
    ctx.env
        .write_file(&output_rel_path, b"")
        .await
        .map_err(|e| StepFailure {
            class: steps::WORKSPACE_CLASS,
            message: format!("could not create the outputs file: {e}"),
        })?;
    env.insert(
        SmolStr::new("GITHUB_OUTPUT"),
        SmolStr::new(format!("{root}/{output_rel}")),
    );
    let allow_unsecure = env
        .get("ACTIONS_ALLOW_UNSECURE_COMMANDS")
        .is_some_and(|v| !v.is_empty() && v != "false" && v != "0");

    let entrypoint = match &config.entrypoint {
        Some(value) => Some(resolve(value)?),
        None => None,
    };
    let args: Vec<SmolStr> = match &config.args_text {
        Some(text) => split_args(&resolve(text)?)
            .into_iter()
            .map(SmolStr::new)
            .collect(),
        None => {
            let mut args = Vec::with_capacity(config.args.len());
            for arg in &config.args {
                args.push(SmolStr::new(resolve(arg)?));
            }
            args
        }
    };

    // GitHub runs the container in `GITHUB_WORKSPACE`; the engine creates the
    // directory when the first checkout has not yet.
    let spec = OneShotContainer {
        image,
        entrypoint: entrypoint.map(|e| SmolStr::new(&e)),
        args,
        env,
        workdir: Some(SmolStr::new(format!("{root}/{REPO_DIR}"))),
    };
    let mut handle = spec_run(&*runner, spec).await?;

    // The command sink watches the container's output for `::` commands, the
    // same contract every GitHub step gets.
    let (tx, rx) = mpsc::channel(64);
    let sink = CommandSink::new(ctx.logs.clone(), ctx.secrets.masker(), allow_unsecure);
    let collected = sink.effects();
    let mut sink_task = tokio::spawn(sink.run(rx));
    let drain = handle.lines().map(|mut lines| {
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                if tx
                    .send(StepEvent::Log {
                        stream: line.stream,
                        line: line.line,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
    });

    let grace = ctx.env.grace();
    let ending = ladder(&mut *handle, &mut ctx.control, grace).await;

    if let Some(drain) = drain {
        let _ = tokio::time::timeout(SINK_LIMIT, drain).await;
    }
    if tokio::time::timeout(SINK_LIMIT, &mut sink_task).await.is_err() {
        sink_task.abort();
        let _ = sink_task.await;
    }
    let commands = std::mem::take(&mut *collected.lock().expect("effects are not poisoned"));

    // What the action wrote to `GITHUB_OUTPUT`, into the outcome's output.
    let mut output = match ctx.env.read_file(&output_rel_path).await {
        Ok(Some(bytes)) => {
            parse_outputs(&String::from_utf8_lossy(&bytes)).map_err(|e| StepFailure {
                class: steps::BAD_OUTPUT_CLASS,
                message: e.to_string(),
            })?
        }
        _ => Map::new(),
    };

    let outcome = match ending {
        Ending::Natural(status) => {
            output.insert("exit_status".into(), exit_value(&status));
            natural_outcome(&status, &config.soft_fail, Value::Object(output))
        }
        Ending::Signalled { escalation, status } => {
            if let Some(status) = &status {
                output.insert("exit_status".into(), exit_value(status));
            }
            output.insert(
                "cancel_escalation".into(),
                Value::String(escalation.to_string()),
            );
            Outcome::new(ir::Status::Cancelled, Value::Object(output))
        }
    };

    let effects = session.finish(commands).await.unwrap_or_default();
    if !effects.summary.is_empty() {
        let _ = ctx
            .logs
            .send(StepEvent::Custom(
                serde_json::json!({ "github/step_summary": effects.summary }),
            ))
            .await;
    }
    let mut state: Map<String, Value> = config
        .state
        .into_iter()
        .map(|(k, v)| (k, Value::String(stringify(&v))))
        .collect();
    state.extend(effects.state.clone());
    Ok(fold_into_outcome(outcome, effects, state))
}

async fn spec_run(
    runner: &dyn executor::ContainerRunner,
    spec: OneShotContainer,
) -> Result<Box<dyn executor::ProcessHandle>, StepFailure> {
    runner.run(spec).await.map_err(|e| StepFailure {
        class: executor::CONTAINER_RUNTIME_CLASS,
        message: format!("could not run the action container: {e}"),
    })
}

/// The image to run, plus `GITHUB_ACTION_REPOSITORY` / `GITHUB_ACTION_REF`.
/// A registry image passes through; a Dockerfile action stages its tree (when
/// pinned) and builds — tagged by the pinned commit, so a tag already built is
/// reused across runs, while a local action rebuilds once per scope.
async fn prepare_image(
    image: &DockerActionImage,
    ctx: &StepCtx,
) -> Result<(ContainerImage, String, String), StepFailure> {
    match image {
        DockerActionImage::Registry(image) => Ok((
            ContainerImage::Registry {
                image: SmolStr::new(image),
            },
            String::new(),
            String::new(),
        )),
        DockerActionImage::Dockerfile(DockerfileImage { action, file }) => {
            validate_relative_action_path(file, false).map_err(|e| StepFailure {
                class: IMAGE_CLASS,
                message: e.to_string(),
            })?;
            let dockerfile = (file != "Dockerfile").then(|| SmolStr::new(file));
            match action {
                crate::config::ActionLocation::Pinned(pinned) => {
                    pinned.validate().map_err(|e| StepFailure {
                        class: IMAGE_CLASS,
                        message: e.to_string(),
                    })?;
                    let source = ctx.require_capability::<ActionSourceCap>()?;
                    let context = stage(ctx, &source.0, pinned).await?;
                    let tag = image_tag(&format!(
                        "{}-{}-{}{}",
                        pinned.reference.owner,
                        pinned.reference.repo,
                        &pinned.sha[..12],
                        pinned
                            .reference
                            .path
                            .as_deref()
                            .map(|p| format!("-{p}"))
                            .unwrap_or_default(),
                    ));
                    Ok((
                        ContainerImage::Build {
                            context,
                            dockerfile,
                            tag: SmolStr::new(tag),
                            reuse: true,
                        },
                        pinned.reference.repository(),
                        pinned.reference.git_ref.to_string(),
                    ))
                }
                crate::config::ActionLocation::Local { local } => {
                    validate_relative_action_path(local, true).map_err(|e| StepFailure {
                        class: IMAGE_CLASS,
                        message: e.to_string(),
                    })?;
                    let tag = image_tag(&format!("local-{local}"));
                    // The tag is not content-addressed: rebuild on the scope's
                    // first use, then the marker lets later phases and steps of
                    // the job reuse it — once per job, as GitHub builds.
                    let marker = PathBuf::from(BUILD_MARKER_DIR).join(&tag);
                    let reuse = matches!(ctx.env.read_file(&marker).await, Ok(Some(_)));
                    if !reuse {
                        let _ = ctx.env.write_file(&marker, b"").await;
                    }
                    Ok((
                        ContainerImage::Build {
                            context: PathBuf::from(REPO_DIR).join(local),
                            dockerfile,
                            tag: SmolStr::new(tag),
                            reuse,
                        },
                        String::new(),
                        String::new(),
                    ))
                }
            }
        }
    }
}

/// A valid, stable Docker image tag from a descriptive key: lowercase
/// alphanumerics with single hyphens between them — the strictest reading of
/// Docker's repository-name grammar, so nothing descriptive can break it.
fn image_tag(key: &str) -> String {
    let mut tag = String::from("petri-action");
    let mut gap = true;
    for c in key.to_lowercase().chars().take(96) {
        if c.is_ascii_alphanumeric() {
            if gap {
                tag.push('-');
                gap = false;
            }
            tag.push(c);
        } else {
            gap = true;
        }
    }
    tag
}

/// The hashes and secrets every configured text may carry, resolved once.
struct TextResolver {
    hashes: std::collections::BTreeMap<Vec<String>, String>,
}

impl TextResolver {
    async fn begin(config: &DockerActionConfig, ctx: &StepCtx) -> Result<Self, StepFailure> {
        let texts: Vec<String> = config
            .entrypoint
            .iter()
            .chain(config.args_text.iter())
            .chain(config.args.iter())
            .chain(config.inputs.values())
            .map(stringify)
            .chain(config.env.values().filter_map(|v| match v {
                ValueOrSecretRef::Literal(value) => Some(stringify(value)),
                ValueOrSecretRef::Secret { .. } => None,
            }))
            .collect();
        let workspace = github_workspace_path(&*ctx.env);
        let hashes = if texts.iter().any(|t| has_hashfiles_sentinel(t)) {
            crate::hashfiles::resolved_calls(texts.iter().map(String::as_str), &*ctx.env, &workspace)
                .await?
        } else {
            Default::default()
        };
        Ok(Self { hashes })
    }

    fn resolve(&self, text: &str, ctx: &StepCtx) -> Result<String, StepFailure> {
        let text = if has_hashfiles_sentinel(text) {
            crate::hashfiles::splice(text, &self.hashes)
        } else {
            text.to_string()
        };
        if !has_secret_sentinel(&text) && !has_sentinel_escape(&text) {
            return Ok(text);
        }
        let resolved = if has_secret_sentinel(&text) {
            replace_secret_sentinels(&text, |name| {
                ctx.secrets
                    .resolve(name)
                    .map(|secret| {
                        frontend_gha::exprs::escape_sentinel_text(&secret.expose())
                    })
                    .map_err(|e| StepFailure {
                        class: steps::SECRET_UNAVAILABLE_CLASS,
                        message: e.to_string(),
                    })
            })?
        } else {
            text
        };
        Ok(unescape_sentinel_text(&resolved))
    }
}

/// Split a `with.args` string into arguments: whitespace separates, single and
/// double quotes group, a backslash escapes inside double quotes and bare text.
fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    current.push(q);
                }
            }
            '"' => {
                started = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        }
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            other => {
                started = true;
                current.push(other);
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

fn exit_value(status: &executor::ExitStatus) -> Value {
    match (status.code, status.signal) {
        (Some(code), _) => Value::from(code),
        (None, Some(signal)) => Value::String(format!("signal:{signal}")),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_split_like_a_shell() {
        assert_eq!(split_args("a b  c"), vec!["a", "b", "c"]);
        assert_eq!(split_args("'a b' c"), vec!["a b", "c"]);
        assert_eq!(split_args(r#""a \"b\"" c"#), vec![r#"a "b""#, "c"]);
        assert_eq!(split_args("one\\ arg"), vec!["one arg"]);
        assert_eq!(split_args("  "), Vec::<String>::new());
        assert_eq!(split_args("''"), vec![""]);
    }

    #[test]
    fn tags_are_valid_and_stable() {
        assert_eq!(
            image_tag("Owner-Repo-0123abcd4567-sub/dir"),
            "petri-action-owner-repo-0123abcd4567-sub-dir"
        );
        assert_eq!(
            image_tag("local-.github/actions/x"),
            "petri-action-local-github-actions-x"
        );
    }
}
