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
use std::convert::Infallible;
use std::path::PathBuf;

use executor::{ContainerImage, OneShotContainer};
use frontend_gha::action::{resolve_manifest_path, validate_relative_action_path};
use frontend_gha::exprs::{
    escape_sentinel_text, has_env_sentinel, has_hashfiles_sentinel, has_runner_temp_sentinel,
    has_runner_tool_cache_sentinel, has_workspace_sentinel, replace_env_sentinels,
    replace_runner_temp_sentinels, replace_runner_tool_cache_sentinels,
    replace_workspace_sentinels, secret_sentinel,
};
use ir::{Outcome, Value};
use serde_json::Map;
use smol_str::SmolStr;
use steps::{Step, StepCtx, StepFailure, ValueOrSecretRef, ending_outcome, ladder, parse_outputs};
use tokio::sync::mpsc;
use tokio::time;

use crate::action::{ActionSourceCap, input_variable, stage};
use crate::commands::CommandSink;
use crate::config::{ActionLocation, DockerActionConfig, DockerActionImage, DockerfileImage};
use crate::session::{
    REPO_DIR, SINK_LIMIT, Session, UNSECURE_COMMANDS_KEY, ci_get, fold_into_outcome, forward_lines,
    github_workspace_path, resolve_sentinel_text, runner_temp_path, settle_sink, stringify,
    unsecure_flag,
};
use crate::{gate, hashfiles};

/// The action's image could not be prepared.
pub(crate) const IMAGE_CLASS: &str = "action_image";

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

async fn execute(mut config: DockerActionConfig, mut ctx: StepCtx) -> Result<Outcome, StepFailure> {
    // The gate first: a phase whose condition is false pulls nothing and
    // creates nothing.
    if let Some(outcome) =
        gate::refusal(config.gate.as_ref(), config.cancelled, &config.env, &ctx).await?
    {
        return Ok(outcome);
    }
    let runner = ctx.require_container_runner()?;
    let mut session = Session::begin(&ctx, &config.event).await?;

    let (image, repository, git_ref) = prepare_image(&config.image, &ctx).await?;

    // Env values first: a bare `${{ env.NAME }}` there resolves from the job's
    // accumulated `GITHUB_ENV` — the environment this container receives — so
    // the resolver below never splices a marker of its own. The action image's
    // own env is out of reach here, so an unbound name renders empty.
    for value in config.env.values_mut() {
        if let ValueOrSecretRef::Literal(Value::String(text)) = value
            && has_env_sentinel(text)
        {
            *text = replace_env_sentinels(text, |name| -> Result<String, Infallible> {
                Ok(ci_get(session.job_env(), name)
                    .map(|v| escape_sentinel_text(v))
                    .unwrap_or_default())
            })
            .expect("the env-file lookup is infallible");
        }
    }

    // Everything textual resolves before the container exists: hashFiles
    // against the workspace, then secret sentinels from the run's provider.
    // The tool cache is this container's own resolution, shared between the
    // sentinel substitution and the exported variable below.
    let container_tool_cache = session.container_tool_cache(&config.env, runner.workspace_path());
    let resolver = TextResolver::begin(
        &config,
        &ctx,
        runner.workspace_path().to_string(),
        container_tool_cache.clone(),
        session.job_env().clone(),
    )
    .await?;
    let resolve = |value: &Value| resolver.resolve(&stringify(value), &ctx);

    let mut env: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
    for (key, value) in &config.env {
        let resolved = match value {
            ValueOrSecretRef::Secret { name } => ctx
                .secrets
                .resolve(name)
                .map_err(|e| StepFailure {
                    class:   steps::SECRET_UNAVAILABLE_CLASS,
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
            SmolStr::new(input_variable(name)),
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
    // The export, from the same resolution the sentinel used — a configured
    // secret is the step's own to keep (its value is already in `env`).
    if !matches!(
        config.env.get("RUNNER_TOOL_CACHE"),
        Some(ValueOrSecretRef::Secret { .. })
    ) {
        env.insert(
            SmolStr::new("RUNNER_TOOL_CACHE"),
            SmolStr::new(&container_tool_cache),
        );
    }
    env.insert(
        SmolStr::new("GITHUB_ACTION_REPOSITORY"),
        SmolStr::new(repository),
    );
    env.insert(SmolStr::new("GITHUB_ACTION_REF"), SmolStr::new(git_ref));
    // The results backend, reached through the runner's guaranteed host alias.
    if let Some(results) = ctx.capability::<crate::ResultsServiceCap>() {
        for (key, value) in results.env(runner.host_address()) {
            env.insert(key, value);
        }
    }
    // The outputs file, created empty so reading it back never depends on the
    // action having written one.
    let output_rel = format!(".ci/out/{}.env", ctx.firing.raw());
    let output_rel_path = PathBuf::from(&output_rel);
    ctx.env
        .write_file(&output_rel_path, b"")
        .await
        .map_err(|e| StepFailure {
            class:   steps::WORKSPACE_CLASS,
            message: format!("could not create the outputs file: {e}"),
        })?;
    env.insert(
        SmolStr::new("GITHUB_OUTPUT"),
        SmolStr::new(format!("{root}/{output_rel}")),
    );
    let allow_unsecure = unsecure_flag(
        env.get(UNSECURE_COMMANDS_KEY).map(SmolStr::as_str),
        &*ctx.env,
    );

    let entrypoint = match &config.entrypoint {
        Some(value) => Some(resolve(value)?),
        None => None,
    };
    let args: Vec<SmolStr> = if let Some(text) = &config.args_text {
        frontend_gha::split_shell_words(&resolve(text)?)
            .into_iter()
            .map(SmolStr::new)
            .collect()
    } else {
        let mut args = Vec::with_capacity(config.args.len());
        for arg in &config.args {
            args.push(SmolStr::new(resolve(arg)?));
        }
        args
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
    let sink_task = tokio::spawn(sink.run(rx));
    let drain = forward_lines(handle.lines(), tx);

    let grace = ctx.env.grace();
    let ending = ladder(&mut *handle, &mut ctx.control, grace).await;

    if let Some(drain) = drain {
        let _ = time::timeout(SINK_LIMIT, drain).await;
    }
    let commands = settle_sink(sink_task, &collected).await;

    // What the action wrote to `GITHUB_OUTPUT`, into the outcome's output.
    let output = match ctx.env.read_file(&output_rel_path).await {
        Ok(Some(bytes)) => {
            parse_outputs(&String::from_utf8_lossy(&bytes)).map_err(|e| StepFailure {
                class:   steps::BAD_OUTPUT_CLASS,
                message: e.to_string(),
            })?
        }
        _ => Map::new(),
    };

    let outcome = ending_outcome(&ending, &config.soft_fail, output);

    let (outcome, effects) = session
        .conclude(outcome, &config.soft_fail, commands, &ctx.logs)
        .await;
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
        class:   executor::CONTAINER_RUNTIME_CLASS,
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
            // The action's own location first — it is the climb budget the
            // Dockerfile path resolves against, so it must hold no `..` itself.
            match action {
                ActionLocation::Pinned(pinned) => {
                    pinned.validate().map_err(|e| StepFailure {
                        class:   IMAGE_CLASS,
                        message: e.to_string(),
                    })?;
                }
                ActionLocation::Local { local } => {
                    validate_relative_action_path(local, true).map_err(|e| StepFailure {
                        class:   IMAGE_CLASS,
                        message: e.to_string(),
                    })?;
                }
            }
            // GitHub builds with the *Dockerfile's parent directory* as the
            // context (the runner joins `runs.image` to the action directory
            // and takes its parent), and manifests depend on it: oss-fuzz's
            // `build_fuzzers.Dockerfile` sits three levels above its actions
            // and does `ADD .` of the directory it sits in. The path resolves
            // like an entry point — `..` bounded by the action's own
            // repository — so the executor receives a normalized context and
            // basename, never `..`.
            let normalized =
                resolve_manifest_path(action.directory(), file).map_err(|e| StepFailure {
                    class:   IMAGE_CLASS,
                    message: e.to_string(),
                })?;
            let (parent, file) = normalized.rsplit_once('/').unwrap_or(("", &normalized));
            let dockerfile = (file != "Dockerfile").then(|| SmolStr::new(file));
            match action {
                ActionLocation::Pinned(pinned) => {
                    let source = ctx.require_capability::<ActionSourceCap>()?;
                    let staged = stage(ctx, &source.0, pinned).await?;
                    let context = join_context(staged, parent);
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
                ActionLocation::Local { local } => {
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
                            context: join_context(PathBuf::from(REPO_DIR), parent),
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

/// A build context under `root`: the Dockerfile's parent directory, which for
/// the common `image: Dockerfile` is the action directory itself.
fn join_context(root: PathBuf, parent: &str) -> PathBuf {
    if parent.is_empty() {
        root
    } else {
        root.join(parent)
    }
}

/// The version of the pipeline that stages and shapes a build context. A
/// pinned build's reuse tag names the action's commit, but the image's bytes
/// also depend on how staging put the context on disk and which directory the
/// build ran in — 71905fb changed file modes, and every image built before it
/// stayed cached and broken, because the tag could not tell the difference.
/// Bump this when the staged bytes or the context derivation change shape; old
/// images become unreferenced instead of immortal. (2 retires everything
/// tagged before versioning existed. 3: the context moved to the Dockerfile's
/// parent directory, as GitHub builds — an `image:` naming a subdirectory's
/// file had built with the action directory as its context, under the same
/// tag.)
const STAGING_VERSION: u32 = 3;

/// A valid, stable Docker image tag from a descriptive key: lowercase
/// alphanumerics with single hyphens between them — the strictest reading of
/// Docker's repository-name grammar, so nothing descriptive can break it.
/// Carries [`STAGING_VERSION`], so the tag names the pipeline as well as the
/// content key.
fn image_tag(key: &str) -> String {
    let mut tag = format!("petri-action-v{STAGING_VERSION}");
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

/// The hashes, runner-side paths and secrets every configured text may carry,
/// resolved once. `container_workspace`, `container_runner_temp` and
/// `container_tool_cache` are the *action container's* view of
/// `GITHUB_WORKSPACE`, `RUNNER_TEMP` and `RUNNER_TOOL_CACHE` — under the
/// mount point, not the job environment's paths.
struct TextResolver {
    hashes:                BTreeMap<Vec<String>, String>,
    container_workspace:   String,
    container_runner_temp: String,
    container_tool_cache:  String,
    /// The phase's `env:` (its env sentinels already resolved) and the job's
    /// accumulated `GITHUB_ENV`: the rungs an `env.NAME` sentinel in an input,
    /// an argument or the entrypoint resolves through — the environment this
    /// container receives. The image's own env is out of reach, so an unbound
    /// name renders empty.
    env_config:            BTreeMap<SmolStr, ValueOrSecretRef>,
    job_env:               BTreeMap<String, String>,
}

impl TextResolver {
    async fn begin(
        config: &DockerActionConfig,
        ctx: &StepCtx,
        container_root: String,
        container_tool_cache: String,
        job_env: BTreeMap<String, String>,
    ) -> Result<Self, StepFailure> {
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
            hashfiles::resolved_calls(texts.iter().map(String::as_str), &*ctx.env, &workspace)
                .await?
        } else {
            BTreeMap::default()
        };
        Ok(Self {
            hashes,
            container_workspace: format!("{container_root}/{REPO_DIR}"),
            container_runner_temp: runner_temp_path(&container_root),
            container_tool_cache,
            env_config: config.env.clone(),
            job_env,
        })
    }

    fn resolve(&self, text: &str, ctx: &StepCtx) -> Result<String, StepFailure> {
        let text = if has_hashfiles_sentinel(text) {
            hashfiles::splice(text, &self.hashes)
        } else {
            text.to_string()
        };
        let text = if has_workspace_sentinel(&text) {
            replace_workspace_sentinels(&text, &self.container_workspace)
        } else {
            text
        };
        let text = if has_runner_temp_sentinel(&text) {
            replace_runner_temp_sentinels(&text, &self.container_runner_temp)
        } else {
            text
        };
        let text = if has_runner_tool_cache_sentinel(&text) {
            replace_runner_tool_cache_sentinels(&text, &self.container_tool_cache)
        } else {
            text
        };
        let text = if has_env_sentinel(&text) {
            replace_env_sentinels(&text, |name| -> Result<String, Infallible> {
                Ok(match ci_get(&self.env_config, name) {
                    Some(ValueOrSecretRef::Literal(v)) => stringify(v),
                    Some(ValueOrSecretRef::Secret { name }) => secret_sentinel(name),
                    None => ci_get(&self.job_env, name)
                        .map(|v| escape_sentinel_text(v))
                        .unwrap_or_default(),
                })
            })
            .expect("the env lookup is infallible")
        } else {
            text
        };
        match resolve_sentinel_text(&text, ctx.secrets.as_ref())? {
            Some(resolved) => Ok(resolved),
            None => Ok(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_valid_and_stable() {
        assert_eq!(
            image_tag("Owner-Repo-0123abcd4567-sub/dir"),
            "petri-action-v3-owner-repo-0123abcd4567-sub-dir"
        );
        assert_eq!(
            image_tag("local-.github/actions/x"),
            "petri-action-v3-local-github-actions-x"
        );
    }

    #[test]
    fn the_context_is_the_dockerfiles_parent() {
        let split = |normalized: &str| {
            let (parent, file) = normalized.rsplit_once('/').unwrap_or(("", normalized));
            (
                join_context(PathBuf::from("root"), parent),
                file.to_string(),
            )
        };
        assert_eq!(
            split("Dockerfile"),
            (PathBuf::from("root"), "Dockerfile".to_string())
        );
        assert_eq!(
            split("sub/dir/Dockerfile"),
            (PathBuf::from("root/sub/dir"), "Dockerfile".to_string())
        );
        // The oss-fuzz shape, resolved: the Dockerfile above its action,
        // built from the directory it sits in.
        assert_eq!(
            split("infra/build_fuzzers.Dockerfile"),
            (
                PathBuf::from("root/infra"),
                "build_fuzzers.Dockerfile".to_string()
            )
        );
    }
}
