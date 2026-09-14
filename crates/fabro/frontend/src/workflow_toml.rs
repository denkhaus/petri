//! `workflow.toml` beside the workflow file: every section Fabro's settings
//! parser accepts, read here into what the standalone runner acts on, warns
//! about, or refuses.
//!
//! The readiness plan's rule applies to every section: a platform-only
//! option warns with why (`ignored.workflow_toml.<section>`), a requirement
//! the runner cannot meet is a specific `unsupported.workflow_toml.*` error
//! before any node runs, and a key Fabro's own parser refuses is an error
//! with Fabro's rename hint. Nothing is dropped silently.
//!
//! What the runner acts on:
//!
//! - `[run.inputs]`: input defaults under the host's `--input`.
//! - `[run] goal`: the run goal when the graph sets none (the graph's `goal`
//!   attribute wins, as in Fabro's run materialization). The `{ file }` form
//!   reads beside `workflow.toml`.
//! - `[run.model]`: the default `provider`, `name`, `reasoning_effort` and
//!   `speed` an agent or prompt node gets when neither it nor the graph sets
//!   one. `[run.model.fallbacks]` is read by [`crate::fallbacks`].
//! - `[run.execution]`: `mode = "dry_run"` and `approval = "auto"` become the
//!   run's launch defaults; `--dry-run` and `--auto-approve` still win.
//! - `[run.environment]` and `[environments.<id>]`: the `provider` selects the
//!   sandbox backend (`local` is the host, `docker` the Docker plugin,
//!   `daytona` the Daytona plugin) when `--backend` is not given;
//!   `image.docker` is the scope's container image under those two; `env` is
//!   the scope environment, with `{{ secrets.NAME }}` as a `$secret` reference
//!   resolved at spawn; `resources` are the Daytona runner size. `network`,
//!   `lifecycle`, `labels`, `cwd` and `image.dockerfile` are platform-only and
//!   warn.
//! - `[run.prepare]`: setup steps that run as command nodes between `start` and
//!   its successors, in the selected environment, before any workflow node.
//!   Each step gets the section's `timeout` (default five minutes) and
//!   `on_failure="exit"`, so a failed step ends the run before the nodes.
//! - `[[run.hooks]]`: the local hook layer beside the workflow, read by
//!   `crate::hooks` together with `.fabro/project.toml` and the host's settings
//!   layer.

use std::collections::BTreeMap;

use frontend::{Diagnostics, FileSource, Span};
use frontend_attractor::model::parse_duration;
use frontend_attractor::template::Context;
use frontend_attractor::{
    DEFAULT_PRESERVE_TURNS, DEFAULT_THRESHOLD_PERCENT, EnvValue, Environment, PrepareStep,
    RunSettings,
};
use serde_json::{Value, json};

use crate::model_layers::LaunchModel;
use crate::secrets::{InterpolationError, interpolate};
use crate::skills;

/// The settings schema version this build reads, Fabro's `_version`.
const WORKFLOW_TOML_VERSION: i64 = 1;

/// Fabro's default timeout for one `[run.prepare]` step.
const PREPARE_TIMEOUT_MS: u64 = 300_000;

/// The `Graph.params` key the launch settings persist under.
pub const LAUNCH_PARAM: &str = "fabro.launch";

/// The `Graph.params` key the resolved environment persists under.
pub const ENVIRONMENT_PARAM: &str = "fabro.environment";

/// Everything `workflow.toml` asks of a run: what the lowering applies
/// ([`RunSettings`]) and what the Fabro launch keeps for itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// What the Attractor lowering applies.
    pub run:          RunSettings,
    /// The launch-level model default the host bound, as given; already
    /// folded into `run.model` below the file layers.
    pub launch:       LaunchModel,
    /// `[run.execution]`.
    pub dry_run:      bool,
    pub auto_approve: bool,
    /// The file's path and text, for the hook and MCP loaders'
    /// `workflow.toml` layer.
    pub hooks_text:   Option<(String, String)>,
}

impl Settings {
    /// The launch settings the persisted graph carries. `repository` is the
    /// absolute root the run checks out when `[run.clone]` is enabled, when
    /// the host bound one.
    pub fn launch_param(&self, repository: Option<&str>) -> Value {
        let environment = self.run.environment.as_ref();
        json!({
            "dry_run": self.dry_run,
            "auto_approve": self.auto_approve,
            "model": self.launch.model,
            "provider": self.launch.provider,
            "clone": {
                "enabled": self.run.clone.enabled,
                "depth": self.run.clone.depth,
                "repository": repository,
            },
            "sandbox_backend": environment.map(Environment::sandbox_backend),
            "cpu_cores": environment.and_then(|e| e.cpu_cores),
            "memory_mb": environment.and_then(|e| e.memory_mb),
            "disk_mb": environment.and_then(|e| e.disk_mb),
        })
    }

    /// The environment record the persisted graph carries, for inspection.
    pub fn environment_param(&self) -> Option<Value> {
        let environment = self.run.environment.as_ref()?;
        Some(json!({
            "id": environment.id,
            "provider": environment.provider,
            "image": environment.image,
            "env": environment.env.iter().map(|(k, v)| (k.clone(), v.to_json())).collect::<serde_json::Map<_, _>>(),
        }))
    }
}

/// Read `workflow.toml` beside `file`. Input defaults land in `template`;
/// everything else comes back as [`Settings`]. Problems are diagnosed.
pub fn read(
    file: &str,
    files: &dyn FileSource,
    template: &mut Context,
    diags: &mut Diagnostics,
) -> Settings {
    let dir = file.rfind('/').map_or("", |i| &file[..i]);
    let path = if dir.is_empty() {
        "workflow.toml".to_string()
    } else {
        format!("{dir}/workflow.toml")
    };
    let Some(text) = files.read(&path) else {
        return Settings::default();
    };
    let value: toml::Table = match text.parse() {
        Ok(value) => value,
        Err(error) => {
            diags.warning(
                "fabro.workflow_toml",
                Span::file(&path),
                format!("`{path}` is not valid TOML and is ignored: {error}"),
            );
            // The hook loader still sees the text: a configured hook in an
            // unparseable file is an error there, never a silent skip.
            return Settings {
                hooks_text: Some((path, text)),
                ..Settings::default()
            };
        }
    };
    let mut reader = Reader {
        path: &path,
        dir,
        span: Span::file(&path),
        files,
        template,
        diags,
        settings: Settings {
            run: RunSettings {
                prepare_timeout_ms: PREPARE_TIMEOUT_MS,
                ..RunSettings::default()
            },
            ..Settings::default()
        },
    };
    reader.top_level(&value);
    let mut settings = reader.settings;
    settings.hooks_text = Some((path, text));
    settings
}

struct Reader<'a> {
    path:     &'a str,
    dir:      &'a str,
    span:     Span,
    files:    &'a dyn FileSource,
    template: &'a mut Context,
    diags:    &'a mut Diagnostics,
    settings: Settings,
}

impl Reader<'_> {
    fn warn(&mut self, code: &str, message: String) {
        self.diags.warning(code, self.span.clone(), message);
    }

    fn unsupported(&mut self, feature: &str, message: String, hint: &str) {
        self.diags
            .unsupported(feature, self.span.clone(), message, hint);
    }

    fn ignored(&mut self, section: &str, why: &str) {
        let path = self.path;
        self.warn(
            &format!("ignored.workflow_toml.{section}"),
            format!("`[{section}]` in `{path}` is ignored: {why}"),
        );
    }

    fn top_level(&mut self, value: &toml::Table) {
        for key in value.keys() {
            if !WORKFLOW_TOML_TOP_LEVEL.contains(&key.as_str()) {
                let hint = rename_hint(key)
                    .unwrap_or("remove it; Fabro's settings schema has no such key");
                let path = self.path;
                self.unsupported(
                    "workflow_toml.key",
                    format!("`{key}` in `{path}` is not a key Fabro's `workflow.toml` accepts"),
                    hint,
                );
            }
        }
        if let Some(version) = value.get("_version").and_then(toml::Value::as_integer)
            && version != WORKFLOW_TOML_VERSION
        {
            let path = self.path;
            self.unsupported(
                "workflow_toml.version",
                format!(
                    "`_version = {version}` in `{path}` is not the settings schema version this \
                     build reads ({WORKFLOW_TOML_VERSION})"
                ),
                "set `_version = 1`",
            );
        }
        for (section, why) in WORKFLOW_TOML_INERT {
            if value.contains_key(*section) {
                self.ignored(section, why);
            }
        }
        if let Some(llm) = value.get("llm").and_then(toml::Value::as_table) {
            for key in WORKFLOW_TOML_LEGACY_LLM_KEYS {
                if llm.contains_key(*key) {
                    let path = self.path;
                    self.unsupported(
                        "workflow_toml.key",
                        format!("`llm.{key}` in `{path}` is a legacy key Fabro refuses"),
                        "rename to `[run.model]`",
                    );
                }
            }
        }
        let environments = value
            .get("environments")
            .and_then(toml::Value::as_table)
            .cloned()
            .unwrap_or_default();
        if let Some(run) = value.get("run").and_then(toml::Value::as_table) {
            self.run_table(run, &environments);
        }
    }

    fn run_table(&mut self, run: &toml::Table, environments: &toml::Table) {
        // Input defaults first: every other section may render `{{ inputs.* }}`.
        if let Some(inputs) = run.get("inputs").and_then(toml::Value::as_table) {
            for (name, value) in inputs {
                let json = serde_json::to_value(value).unwrap_or(Value::Null);
                self.template.default_input(name, json);
            }
        }
        for (key, item) in run {
            match key.as_str() {
                // Inputs were read above. `[[run.hooks]]` is read by
                // `lower::hooks`, with the project and settings layers, from
                // the text kept on the settings.
                "inputs" | "hooks" => {}
                "goal" => self.goal(item),
                "model" => self.model(item),
                "execution" => self.execution(item),
                "environment" => self.environment(item, environments),
                "prepare" => self.prepare(item),
                "agent" => self.agent(item),
                "clone" => self.clone_section(item),
                other => self.other_run_key(other),
            }
        }
    }

    /// A `[run]` key with no reader of its own: a platform-only section that
    /// warns, or a key Fabro's parser refuses.
    fn other_run_key(&mut self, key: &str) {
        let path = self.path;
        if let Some((section, why)) = RUN_SECTIONS_IGNORED
            .iter()
            .find(|(section, _)| *section == key)
        {
            self.warn(
                &format!("ignored.workflow_toml.run.{section}"),
                format!("`[run.{section}]` in `{path}` is ignored: {why}"),
            );
        } else {
            self.unsupported(
                "workflow_toml.key",
                format!("`run.{key}` in `{path}` is not a key Fabro's `[run]` table accepts"),
                "remove it; Fabro's settings schema has no such key",
            );
        }
    }

    /// `goal = "text"` or `[run.goal] file = "path"`.
    fn goal(&mut self, item: &toml::Value) {
        let text = match item {
            toml::Value::String(text) => text.clone(),
            toml::Value::Table(table) => {
                let Some(file) = table.get("file").and_then(toml::Value::as_str) else {
                    self.unsupported(
                        "workflow_toml.key",
                        format!(
                            "`[run.goal]` in `{}` must be a string or `{{ file = ... }}`",
                            self.path
                        ),
                        "write `goal = \"...\"` or `goal = { file = \"goal.md\" }`",
                    );
                    return;
                };
                let path = if self.dir.is_empty() {
                    file.to_string()
                } else {
                    format!("{}/{file}", self.dir)
                };
                let Some(text) = self.files.read(&path) else {
                    self.diags.error(
                        "fabro.file_not_found",
                        self.span.clone(),
                        format!(
                            "`[run.goal] file = \"{file}\"` names `{path}`, which cannot be read"
                        ),
                    );
                    return;
                };
                text
            }
            _ => {
                self.unsupported(
                    "workflow_toml.key",
                    format!(
                        "`run.goal` in `{}` must be a string or `{{ file = ... }}`",
                        self.path
                    ),
                    "write `goal = \"...\"`",
                );
                return;
            }
        };
        if let Some(rendered) = self.render(&text, "`[run] goal`") {
            self.settings.run.goal = Some(rendered);
        }
    }

    /// Render `{{ inputs.* }}`, `{{ vars.* }}` and `{{ goal }}` in a settings
    /// string. `{{ secrets.* }}` is refused here: only an environment value
    /// may carry one.
    fn render(&mut self, text: &str, what: &str) -> Option<String> {
        match interpolate(text, self.template, false) {
            Ok(rendered) => Some(rendered.text),
            Err(InterpolationError::SecretNotAllowed { name }) => {
                self.unsupported(
                    "workflow_toml.secret_position",
                    format!(
                        "{what} in `{}` reads `{{{{ secrets.{name} }}}}`; a secret may appear only \
                         as an environment value, never in text that reaches a log",
                        self.path
                    ),
                    "move the secret to `env` and read the variable in the script",
                );
                None
            }
            Err(InterpolationError::Unbound { name }) => {
                self.unsupported(
                    "template.unbound_input",
                    format!(
                        "{what} in `{}` reads `{{{{ {name} }}}}`, which no input binds",
                        self.path
                    ),
                    &format!(
                        "pass `--input {}=VALUE`, or add a default under `[run.inputs]`",
                        name.strip_prefix("inputs.").unwrap_or(&name)
                    ),
                );
                None
            }
            Err(InterpolationError::Env { name }) => {
                self.unsupported(
                    "workflow_toml.env_token",
                    format!(
                        "{what} in `{}` reads `{{{{ env.{name} }}}}`, which Fabro parses but never \
                         resolves",
                        self.path
                    ),
                    "use `{{ inputs.NAME }}` or `{{ secrets.NAME }}`",
                );
                None
            }
        }
    }

    /// An environment value: literal text, or exactly `{{ secrets.NAME }}`.
    fn env_value(&mut self, section: &str, key: &str, value: &toml::Value) -> Option<EnvValue> {
        let Some(text) = value.as_str() else {
            self.unsupported(
                "workflow_toml.key",
                format!("`{section}.env.{key}` in `{}` must be a string", self.path),
                "write the value as a string",
            );
            return None;
        };
        match interpolate(text, self.template, true) {
            Ok(rendered) => match rendered.secret {
                Some(name) => Some(EnvValue::Secret(name)),
                None => Some(EnvValue::Literal(rendered.text)),
            },
            Err(InterpolationError::SecretNotAllowed { name }) => {
                self.unsupported(
                    "workflow_toml.secret_position",
                    format!(
                        "`{section}.env.{key}` in `{}` mixes `{{{{ secrets.{name} }}}}` with other \
                         text; a secret must be the whole value",
                        self.path
                    ),
                    "write `KEY = \"{{ secrets.NAME }}\"` on its own",
                );
                None
            }
            Err(InterpolationError::Unbound { name }) => {
                self.unsupported(
                    "template.unbound_input",
                    format!(
                        "`{section}.env.{key}` in `{}` reads `{{{{ {name} }}}}`, which no input binds",
                        self.path
                    ),
                    "pass the input, or add a default under `[run.inputs]`",
                );
                None
            }
            Err(InterpolationError::Env { name }) => {
                self.unsupported(
                    "workflow_toml.env_token",
                    format!(
                        "`{section}.env.{key}` in `{}` reads `{{{{ env.{name} }}}}`, which Fabro \
                         parses but never resolves",
                        self.path
                    ),
                    "use `{{ inputs.NAME }}` or `{{ secrets.NAME }}`",
                );
                None
            }
        }
    }

    fn model(&mut self, item: &toml::Value) {
        let Some(model) = item.as_table() else {
            return;
        };
        for (key, value) in model {
            match key.as_str() {
                "fallbacks" => {
                    self.settings.run.model.fallbacks =
                        super::fallbacks::read(self.path, self.diags, value);
                }
                "provider" => self.settings.run.model.provider = value.as_str().map(str::to_owned),
                "name" => self.settings.run.model.name = value.as_str().map(str::to_owned),
                "controls" => {
                    let Some(controls) = value.as_table() else {
                        continue;
                    };
                    if let Some(effort) = controls
                        .get("reasoning_effort")
                        .and_then(toml::Value::as_str)
                    {
                        self.settings.run.model.reasoning_effort = Some(effort.to_owned());
                    }
                    if let Some(speed) = controls.get("speed").and_then(toml::Value::as_str) {
                        self.settings.run.model.speed = Some(speed.to_owned());
                    }
                }
                other => {
                    let path = self.path;
                    self.unsupported(
                        "workflow_toml.key",
                        format!("`run.model.{other}` in `{path}` is not a key Fabro accepts"),
                        "use `provider`, `name`, `controls` or `fallbacks`",
                    );
                }
            }
        }
    }

    /// `[run.clone]`: `enabled` and `depth`, as Fabro's `RunCloneLayer`
    /// reads them. A negative depth is Fabro's full history (0).
    fn clone_section(&mut self, item: &toml::Value) {
        let Some(clone) = item.as_table() else {
            return;
        };
        for (key, value) in clone {
            match (key.as_str(), value) {
                ("enabled", toml::Value::Boolean(enabled)) => {
                    self.settings.run.clone.enabled = *enabled;
                }
                ("depth", toml::Value::Integer(depth)) => {
                    self.settings.run.clone.depth = (*depth).max(0);
                }
                ("enabled" | "depth", _) => {
                    let path = self.path;
                    self.unsupported(
                        "workflow_toml.key",
                        format!("`run.clone.{key}` in `{path}` has the wrong type"),
                        "`enabled` is a boolean, `depth` an integer",
                    );
                }
                (other, _) => {
                    let path = self.path;
                    self.unsupported(
                        "workflow_toml.key",
                        format!("`run.clone.{other}` in `{path}` is not a key Fabro accepts"),
                        "use `enabled` or `depth`",
                    );
                }
            }
        }
    }

    fn execution(&mut self, item: &toml::Value) {
        let Some(execution) = item.as_table() else {
            return;
        };
        for (key, value) in execution {
            match (key.as_str(), value.as_str()) {
                ("mode", Some("normal")) | ("approval", Some("prompt")) => {}
                ("mode", Some("dry_run")) => self.settings.dry_run = true,
                ("approval", Some("auto")) => self.settings.auto_approve = true,
                ("mode", _) => self.unsupported(
                    "workflow_toml.key",
                    format!(
                        "`run.execution.mode` in `{}` must be `normal` or `dry_run`",
                        self.path
                    ),
                    "set `mode = \"normal\"` or `mode = \"dry_run\"`",
                ),
                ("approval", _) => self.unsupported(
                    "workflow_toml.key",
                    format!(
                        "`run.execution.approval` in `{}` must be `prompt` or `auto`",
                        self.path
                    ),
                    "set `approval = \"prompt\"` or `approval = \"auto\"`",
                ),
                (other, _) => self.unsupported(
                    "workflow_toml.key",
                    format!(
                        "`run.execution.{other}` in `{}` is not a key Fabro accepts",
                        self.path
                    ),
                    "use `mode` or `approval`",
                ),
            }
        }
    }

    fn agent(&mut self, item: &toml::Value) {
        let Some(agent) = item.as_table() else {
            return;
        };
        // Fabro's `[run.agent]` accepts `fabro_tools` and `mcps` only. Neither
        // sub-agents (readiness item 9d) nor compaction (item 9e) has a
        // `workflow.toml` surface at the pinned revision: every native agent
        // gets the sub-agent tools (`super::subagents`), and compaction is
        // always on with Fabro's hardcoded values (`lower::compaction`), so a
        // key asking for either is refused as Fabro refuses it, never passed
        // silently. `skills` is the standalone
        // runner's own extension (`skills::read`).
        for key in agent.keys() {
            if !matches!(key.as_str(), "fabro_tools" | "mcps" | "skills") {
                let path = self.path;
                let (message, hint) = if key == "compaction" {
                    (
                        format!(
                            "`run.agent.compaction` in `{path}` is not a key Fabro's `[run.agent]` \
                             table accepts (it takes `fabro_tools` and `mcps`); compaction is \
                             always on with Fabro's values: a trigger at \
                             {DEFAULT_THRESHOLD_PERCENT} percent of the context window and \
                             {DEFAULT_PRESERVE_TURNS} preserved turns"
                        ),
                        "remove it; Fabro's settings schema has no such key",
                    )
                } else if key == "subagents" {
                    (
                        format!(
                            "`run.agent.subagents` in `{path}` is not a key Fabro's `[run.agent]` \
                             table accepts (it takes `fabro_tools` and `mcps`); sub-agents are \
                             always available to native agents"
                        ),
                        "remove it; native agents always have the sub-agent tools, as in Fabro, \
                         and Fabro's settings schema has no such key",
                    )
                } else {
                    (
                        format!(
                            "`run.agent.{key}` in `{path}` is not a key Fabro's `[run.agent]` \
                             table accepts (it takes `fabro_tools` and `mcps`)"
                        ),
                        "remove it; Fabro's settings schema has no such key",
                    )
                };
                self.unsupported("workflow_toml.key", message, hint);
            }
        }
        if let Some(value) = agent.get("skills") {
            self.settings.run.skills = skills::read(value, self.path, &self.span, self.diags);
        }
        if agent.get("fabro_tools").and_then(toml::Value::as_bool) == Some(true) {
            let path = self.path;
            self.warn(
                "ignored.workflow_toml.run.agent.fabro_tools",
                format!(
                    "`fabro_tools = true` in `{path}` is ignored: run-management tools are a \
                     Fabro platform facility; agents get the sandbox tools only"
                ),
            );
        }
        // `[run.agent.mcps]` is read by `lower::mcps` from this file's text,
        // together with the other settings layers.
    }

    /// `[run.environment]` over `[environments.<id>]`, Fabro's `combine`: the
    /// run's fields win, the named environment fills the rest.
    fn environment(&mut self, item: &toml::Value, environments: &toml::Table) {
        let Some(run_env) = item.as_table() else {
            return;
        };
        let id = run_env
            .get("id")
            .and_then(toml::Value::as_str)
            .unwrap_or("default")
            .to_string();
        let base = environments.get(&id).and_then(toml::Value::as_table);
        let Some(base) = base else {
            if run_env.keys().any(|k| k != "id") || id != "default" {
                self.unsupported(
                    "workflow_toml.run.environment",
                    format!(
                        "`[run.environment] id = \"{id}\"` in `{}` names no `[environments.{id}]` \
                         table",
                        self.path
                    ),
                    &format!("add `[environments.{id}]` with a `provider`"),
                );
            }
            return;
        };
        let provider = base
            .get("provider")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        if !matches!(provider.as_str(), "local" | "docker" | "daytona") {
            self.unsupported(
                "workflow_toml.environments.provider",
                format!(
                    "`[environments.{id}] provider = \"{provider}\"` in `{}` is not `local`, \
                     `docker` or `daytona`",
                    self.path
                ),
                "set the provider to `local`, `docker` or `daytona`",
            );
            return;
        }
        let field = |key: &str| run_env.get(key).or_else(|| base.get(key));
        let mut environment = Environment {
            id:        id.clone(),
            provider:  provider.clone(),
            image:     None,
            env:       BTreeMap::new(),
            cpu_cores: None,
            memory_mb: None,
            disk_mb:   None,
        };
        if let Some(image) = field("image").and_then(toml::Value::as_table) {
            if let Some(docker) = image.get("docker").and_then(toml::Value::as_str) {
                if provider == "local" {
                    self.unsupported(
                        "workflow_toml.environments.image",
                        format!(
                            "`[environments.{id}] image.docker` in `{}` names a container image, \
                             but the `local` provider runs on the host",
                            self.path
                        ),
                        "use `provider = \"docker\"`, or drop the image",
                    );
                } else {
                    environment.image = Some(docker.to_string());
                }
            }
            if image.contains_key("dockerfile") {
                // Fabro builds the image on its platform. The standalone
                // runner has no image build; the run uses the selected
                // backend's default runner image, and says so.
                self.ignored(
                    &format!("environments.{id}.image.dockerfile"),
                    "the standalone runner does not build images; the scope runs on the selected \
                     backend's default runner image (build the image yourself and name it with \
                     `image.docker` to use it)",
                );
            }
        }
        if let Some(cwd) = base.get("cwd") {
            let _ = cwd;
            self.ignored(
                &format!("environments.{id}.cwd"),
                "the working directory is the sandbox workspace the run was given",
            );
        }
        if let Some(resources) = field("resources").and_then(toml::Value::as_table) {
            if provider == "daytona" {
                environment.cpu_cores = resources
                    .get("cpu")
                    .and_then(toml::Value::as_integer)
                    .and_then(|n| u32::try_from(n).ok());
                environment.memory_mb = resources.get("memory").and_then(size_mb);
                environment.disk_mb = resources.get("disk").and_then(size_mb);
            } else {
                self.ignored(
                    &format!("environments.{id}.resources"),
                    "resource limits apply to a Daytona runner; the host and Docker providers \
                     run unconstrained",
                );
            }
        }
        for (key, why) in [
            ("network", "network policy is a Fabro platform facility"),
            (
                "lifecycle",
                "sandbox lifecycle is decided by `--retain` and the run's teardown",
            ),
            ("labels", "sandbox labels are a Fabro platform record"),
        ] {
            if field(key).is_some() {
                self.ignored(&format!("environments.{id}.{key}"), why);
            }
        }
        // `env`: the named environment's values under the run's, Fabro's
        // sticky map order.
        let mut merged: Vec<(String, toml::Value, String)> = Vec::new();
        if let Some(env) = base.get("env").and_then(toml::Value::as_table) {
            for (key, value) in env {
                merged.push((key.clone(), value.clone(), format!("environments.{id}")));
            }
        }
        if let Some(env) = run_env.get("env").and_then(toml::Value::as_table) {
            for (key, value) in env {
                merged.retain(|(k, _, _)| k != key);
                merged.push((key.clone(), value.clone(), "run.environment".to_string()));
            }
        }
        for (key, value, section) in merged {
            if let Some(value) = self.env_value(&section, &key, &value) {
                environment.env.insert(key, value);
            }
        }
        self.settings.run.environment = Some(environment);
    }

    fn prepare(&mut self, item: &toml::Value) {
        let Some(prepare) = item.as_table() else {
            return;
        };
        if let Some(timeout) = prepare.get("timeout") {
            match timeout.as_str().and_then(parse_duration) {
                Some(duration) => {
                    self.settings.run.prepare_timeout_ms =
                        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                }
                None => self.unsupported(
                    "workflow_toml.key",
                    format!(
                        "`run.prepare.timeout` in `{}` must be a duration such as `5m`",
                        self.path
                    ),
                    "write the timeout with a unit",
                ),
            }
        }
        for key in prepare.keys() {
            if !matches!(key.as_str(), "steps" | "timeout") {
                let path = self.path;
                self.unsupported(
                    "workflow_toml.key",
                    format!("`run.prepare.{key}` in `{path}` is not a key Fabro accepts"),
                    "use `steps` and `timeout`",
                );
            }
        }
        let Some(steps) = prepare.get("steps").and_then(toml::Value::as_array) else {
            return;
        };
        for (index, step) in steps.iter().enumerate() {
            let Some(step) = step.as_table() else {
                self.unsupported(
                    "workflow_toml.run.prepare",
                    format!(
                        "`run.prepare.steps[{index}]` in `{}` must be a table",
                        self.path
                    ),
                    "write `[[run.prepare.steps]]` with `script` or `command`",
                );
                continue;
            };
            let script = step.get("script").and_then(toml::Value::as_str);
            let command = step.get("command").and_then(toml::Value::as_array);
            let text = match (script, command) {
                (Some(script), None) => {
                    self.render(script, &format!("`run.prepare.steps[{index}].script`"))
                }
                (None, Some(argv)) => {
                    let mut words = Vec::with_capacity(argv.len());
                    let mut ok = true;
                    for (position, word) in argv.iter().enumerate() {
                        let Some(word) = word.as_str() else {
                            ok = false;
                            self.unsupported(
                                "workflow_toml.run.prepare",
                                format!(
                                    "`run.prepare.steps[{index}].command[{position}]` in `{}` must \
                                     be a string",
                                    self.path
                                ),
                                "write every argument as a string",
                            );
                            continue;
                        };
                        match self.render(word, &format!("`run.prepare.steps[{index}].command`")) {
                            Some(rendered) => words.push(rendered),
                            None => ok = false,
                        }
                    }
                    ok.then(|| shlex::try_join(words.iter().map(String::as_str)).ok())
                        .flatten()
                }
                _ => {
                    self.unsupported(
                        "workflow_toml.run.prepare",
                        format!(
                            "`run.prepare.steps[{index}]` in `{}`: exactly one of script or command \
                             must be set",
                            self.path
                        ),
                        "give the step a `script` or a `command`, not both",
                    );
                    None
                }
            };
            let Some(text) = text else {
                continue;
            };
            let mut env = BTreeMap::new();
            if let Some(table) = step.get("env").and_then(toml::Value::as_table) {
                for (key, value) in table {
                    if let Some(value) =
                        self.env_value(&format!("run.prepare.steps[{index}]"), key, value)
                    {
                        env.insert(key.clone(), value);
                    }
                }
            }
            for key in step.keys() {
                if !matches!(key.as_str(), "script" | "command" | "env") {
                    let path = self.path;
                    self.unsupported(
                        "workflow_toml.key",
                        format!("`run.prepare.steps[{index}].{key}` in `{path}` is not a key Fabro accepts"),
                        "use `script` or `command`, and `env`",
                    );
                }
            }
            self.settings
                .run
                .prepare
                .push(PrepareStep { script: text, env });
        }
    }
}

/// Fabro's size grammar: an integer with `B`, `KB`, `MB`, `GB`, `TB` (or
/// the `iB` forms), a bare integer meaning gigabytes; in MiB.
fn size_mb(value: &toml::Value) -> Option<u64> {
    let bytes = match value {
        toml::Value::Integer(n) => u64::try_from(*n).ok()?.checked_mul(1024 * 1024 * 1024)?,
        toml::Value::String(text) => {
            let trimmed = text.trim();
            let split = trimmed
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(trimmed.len());
            let number: u64 = trimmed[..split].parse().ok()?;
            let multiplier: u64 = match trimmed[split..].trim().to_ascii_uppercase().as_str() {
                "" | "GB" | "GIB" => 1024 * 1024 * 1024,
                "B" => 1,
                "KB" | "KIB" => 1024,
                "MB" | "MIB" => 1024 * 1024,
                "TB" | "TIB" => 1024_u64.pow(4),
                _ => return None,
            };
            number.checked_mul(multiplier)?
        }
        _ => return None,
    };
    Some(bytes / (1024 * 1024))
}

/// The top-level keys Fabro's settings parser accepts; anything else is a
/// hard error there and here.
const WORKFLOW_TOML_TOP_LEVEL: &[&str] = &[
    "_version",
    "project",
    "workflow",
    "environments",
    "run",
    "cli",
    "server",
    "llm",
];

/// Legacy `[llm]` keys Fabro refuses with a rename hint.
const WORKFLOW_TOML_LEGACY_LLM_KEYS: &[&str] = &[
    "provider",
    "model",
    "temperature",
    "max_tokens",
    "fallbacks",
    "fallback",
];

/// Top-level sections that are accepted in a workflow file but carry nothing
/// the standalone runner acts on.
const WORKFLOW_TOML_INERT: &[(&str, &str)] = &[
    (
        "project",
        "project settings belong to `.fabro/project.toml`; the standalone runner reads none",
    ),
    (
        "cli",
        "Fabro CLI settings do not apply to the standalone runner",
    ),
    (
        "server",
        "Fabro server settings do not apply to the standalone runner",
    ),
    (
        "llm",
        "the provider catalog comes from the distribution and `PETRI_LLM_CATALOG`, not from \
         the workflow file",
    ),
];

/// `[run.*]` sections the standalone runner reads but does not act on, each
/// with why.
const RUN_SECTIONS_IGNORED: &[(&str, &str)] = &[
    (
        "working_dir",
        "the working directory is the sandbox workspace the run was given",
    ),
    ("metadata", "run metadata is a Fabro platform record"),
    (
        "run_branch",
        "the standalone runner performs no Git operations of its own",
    ),
    (
        "meta_branch",
        "the standalone runner performs no Git operations of its own",
    ),
    (
        "pull_request",
        "the standalone runner performs no Git operations of its own",
    ),
    (
        "git",
        "the standalone runner performs no Git operations of its own",
    ),
    (
        "integrations",
        "platform integrations are supplied by an embedding host, not the standalone runner; \
         the run inherits the ambient `GITHUB_TOKEN` or none",
    ),
    (
        "checkpoint",
        "the standalone runner does not checkpoint the workspace; it retains it instead",
    ),
    (
        "artifacts",
        "artifact selection is not implemented yet; the whole retained workspace is the result",
    ),
    (
        "notifications",
        "notification routes are a Fabro platform facility",
    ),
    (
        "interviews",
        "interview routing is a Fabro platform facility; the host's interviewer answers",
    ),
    ("scm", "SCM metadata is a Fabro platform record"),
];

/// Fabro's rename hint for a top-level key its parser refuses.
fn rename_hint(key: &str) -> Option<&'static str> {
    Some(match key {
        "version" => "rename to `_version`",
        "goal" | "goal_file" | "work_dir" | "directory" => "move to `[run]`",
        "graph" => "move to `[workflow]`",
        "labels" => "move to `[run.metadata]`",
        "vars" => "rename to `[run.inputs]`",
        "setup" => "rename to `[run.prepare]`",
        "sandbox" => "rename to `[run.environment]` and `[environments.<slug>]`",
        "checkpoint" => "move under `[run.checkpoint]`",
        "pull_request" => "move under `[run.pull_request]`",
        "artifacts" => "move under `[run.artifacts]`",
        "hooks" => "move under `[[run.hooks]]`",
        "mcp_servers" => "move under `[run.agent.mcps.<name>]`",
        "exec" => "rename to `[cli.exec]`",
        "api" => "rename to `[server.api]`",
        "web" => "rename to `[server.web]`",
        "artifact_storage" => "rename to `[server.artifacts]`",
        "storage_dir" | "data_dir" => "rename to `[server.storage] root`",
        "max_concurrent_runs" => "rename to `[server.scheduler]`",
        "fabro" => "rename to `[project]`",
        "git" => "split into `[run.git]` and `[server.integrations.github]`",
        "github" => {
            "split into `[server.integrations.github]` and `[run.integrations.github.permissions]`"
        }
        "slack" => "move under `[server.integrations.slack]`",
        "log" => "rename to `[server.logging]` or `[cli.logging]`",
        "prevent_idle_sleep" => "rename to `[cli.exec] prevent_idle_sleep`",
        "verbose" => "rename to `[cli.output] verbosity`",
        "upgrade_check" => "rename to `[cli.updates] check`",
        "dry_run" => "rename to `[run.execution] mode = \"dry_run\"`",
        "auto_approve" => "rename to `[run.execution] approval = \"auto\"`",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_follow_fabro() {
        assert_eq!(
            size_mb(&toml::Value::String("16GB".into())),
            Some(16 * 1024)
        );
        assert_eq!(size_mb(&toml::Value::String("512MiB".into())), Some(512));
        assert_eq!(size_mb(&toml::Value::Integer(2)), Some(2048));
        assert_eq!(size_mb(&toml::Value::String("1.5GB".into())), None);
    }
}
