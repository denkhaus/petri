//! A job's scope: `runs-on` placement and `container`, and the environment
//! GitHub gives every step of the job.

use std::mem;

use frontend::diag::Span;
use frontend::yaml::{Mapping, Node};
use ir::{ExprOrValue, RuntimeSpec, Scope, ScopeId, Value};
use serde_json::json;
use smol_str::SmolStr;

use super::{EnvValue, Lowering};
use crate::model::{Job, Workflow};
use crate::runners::{self, LabelClass};
use crate::{exprs, runs_on};

/// The job container's `env:` entries, when the job has a container mapping.
/// Read here for the scope and again by `job_body`'s secret scan, so a secret
/// in container env is pushed down into every step like a job-env secret.
/// The scope-level env in precedence order: container env first (lowest — it
/// configures the container, and per-step env wins inside it), then workflow
/// env, then job env on top. The one spelling of that order, shared by the
/// scope's env and `job_body`'s secret push-down.
pub(super) fn scope_env<'x>(wf: &Workflow<'x>, job: &Job<'x>) -> Vec<(String, Node<'x>)> {
    let mut entries: Vec<(String, Node<'x>)> = job
        .container
        .and_then(|c| c.as_mapping())
        .and_then(|m| m.get("env"))
        .and_then(|e| e.as_mapping())
        .map(|em| em.iter().map(|(k, v)| (k.to_string(), v)).collect())
        .unwrap_or_default();
    entries.extend(
        wf.env
            .iter()
            .chain(job.env.iter())
            .map(|(k, n)| (k.clone(), *n)),
    );
    entries
}

impl<'a> Lowering<'_, 'a> {
    pub(super) fn scope_for(&mut self, job: &Job<'a>) -> ScopeId {
        let mut scope = Scope::new(ScopeId::new(0));
        scope.runtime = self.runtime_for(job);
        scope.services = self.services_for(job);

        // Default environment GitHub gives every step, from the run parameters.
        let site = self.base_site(job);
        let github = |this: &mut Self, key: &str| -> ExprOrValue {
            let t = this.b.exprs();
            let ctx = t.var("github");
            let k = t.lit(key);
            let v = t.call("get_ci", vec![ctx, k]);
            let s = t.call("loose_string", vec![v]);
            ExprOrValue::Expr(s)
        };
        scope
            .env
            .insert("CI".into(), ExprOrValue::Value(json!("true")));
        scope
            .env
            .insert("GITHUB_ACTIONS".into(), ExprOrValue::Value(json!("true")));
        scope
            .env
            .insert("GITHUB_JOB".into(), ExprOrValue::Value(json!(job.id)));
        if let Some(name) = &self.wf.name {
            scope
                .env
                .insert("GITHUB_WORKFLOW".into(), ExprOrValue::Value(json!(name)));
        }
        for key in [
            "sha",
            "ref",
            "ref_name",
            "ref_type",
            "repository",
            "repository_owner",
            "actor",
            "event_name",
            "run_id",
            "run_number",
            "run_attempt",
            "server_url",
            "api_url",
            "base_ref",
            "head_ref",
        ] {
            let var = format!("GITHUB_{}", key.to_uppercase());
            scope.env.insert(SmolStr::new(&var), github(self, key));
        }
        // Only the runner facts the params actually carry. `RUNNER_TEMP` and
        // `RUNNER_TOOL_CACHE` are the session's per-step business — an
        // empty-string value here would *override* a container image's own
        // (the runner images name their populated `/opt/hostedtoolcache`).
        for key in ["os", "arch", "name"] {
            let t = self.b.exprs();
            let ctx = t.var("runner");
            let k = t.lit(key);
            let v = t.call("get_ci", vec![ctx, k]);
            let s = t.call("loose_string", vec![v]);
            scope.env.insert(
                SmolStr::new(format!("RUNNER_{}", key.to_uppercase())),
                ExprOrValue::Expr(s),
            );
        }

        // Secret refs cannot live in scope env (there is nowhere for them
        // to go but the process), so they are pushed down into every step's env
        // instead; `job_body` reads them back from `site`.
        for (key, node) in scope_env(self.wf, job) {
            match self.env_value(&node, &site, exprs::ExprSite::Job) {
                Some(EnvValue::Plain(v)) => {
                    scope.env.insert(SmolStr::new(&key), v);
                }
                Some(EnvValue::Secret(_)) | None => {
                    // A secret is recorded in job_body via the same lookup, and
                    // a value that did not lower already reported itself.
                }
            }
        }
        self.b.add_scope(scope)
    }

    fn runtime_for(&mut self, job: &Job<'a>) -> RuntimeSpec {
        let mut spec = RuntimeSpec::host_process();

        match job.runs_on {
            None if job.call.is_some() => {}
            None => {
                self.diags.error(
                    "gha.no_runs_on",
                    job.span.clone(),
                    format!("job `{}` has no `runs-on`", job.id),
                );
            }
            Some(node) => {
                let raw: Vec<runs_on::RawLabel> =
                    if let Some(label) = runs_on::RawLabel::try_from_node(node, true) {
                        vec![label]
                    } else if let Some(seq) = node.as_sequence() {
                        seq.iter()
                            .filter_map(|n| runs_on::RawLabel::try_from_node(n, false))
                            .collect()
                    } else if let Some(m) = node.as_mapping() {
                        // `runs-on: { group: …, labels: … }`
                        let mut out = Vec::new();
                        if let Some(labels) = m.get("labels") {
                            if let Some(one) = runs_on::RawLabel::try_from_node(labels, false) {
                                out.push(one);
                            } else if let Some(seq) = labels.as_sequence() {
                                out.extend(
                                    seq.iter()
                                        .filter_map(|n| runs_on::RawLabel::try_from_node(n, false)),
                                );
                            }
                        }
                        if let Some(group) = m.get("group") {
                            self.diags.unsupported(
                                "runs_on.group",
                                group.span(),
                                "`runs-on.group` names a runner group",
                                "runner groups are a GitHub-hosted concept; name a label instead",
                            );
                        }
                        out
                    } else {
                        Vec::new()
                    };
                if raw.iter().any(|l| l.text.contains("${{")) {
                    self.expression_runs_on(job, node, &raw, &mut spec);
                } else {
                    for label in raw {
                        self.check_label(&mut spec, &label.text, label.span, None);
                    }
                }
            }
        }

        if let Some(container) = job.container {
            let mut image_node = None;
            let mut options: Vec<SmolStr> = Vec::new();
            let mut credentials = None;
            if container.as_str().is_some() {
                image_node = Some(container);
            } else if let Some(m) = container.as_mapping() {
                for (key, span) in m.keys() {
                    match key {
                        "image" | "env" | "options" | "credentials" => {}
                        other => self.diags.unsupported(
                            &format!("container.{other}"),
                            span,
                            format!("`container.{other}`"),
                            "`image`, `env`, `options` and `credentials` map onto a container \
                             scope",
                        ),
                    }
                }
                image_node = m.get("image");
                if let Some(node) = m.get("options") {
                    options = self.engine_flags(node, "container");
                }
                if let Some(node) = m.get("credentials") {
                    credentials = self.registry_credentials(node, "container");
                }
            }
            match image_node.and_then(|n| n.as_str().map(|s| (s.to_string(), n.span()))) {
                Some((text, span)) => {
                    if let Some(image) = self.static_scope_text(&text, span, "container image") {
                        let requirements = mem::take(&mut spec.requirements);
                        spec = RuntimeSpec::container(&image);
                        spec.requirements = requirements;
                        if let ir::RuntimeTarget::Container {
                            options: target_options,
                            credentials: target_credentials,
                            ..
                        } = &mut spec.target
                        {
                            *target_options = options;
                            *target_credentials = credentials;
                        }
                    }
                }
                None => self.diags.error(
                    "gha.bad_container",
                    container.span(),
                    "`container` needs an `image`",
                ),
            }
        }
        spec
    }

    /// A per-scope text — a container image, a registry username: literal, or
    /// an expression over the static contexts (the frame's known `inputs` and
    /// the checkout's `github` identity, never `matrix`: every leg shares the
    /// scope). `None` reports the specific rejection.
    fn static_scope_text(&mut self, text: &str, span: Span, what: &str) -> Option<String> {
        if !text.contains("${{") {
            return Some(text.to_string());
        }
        let inputs = self.placement_inputs();
        let resolved = runs_on::static_scalar(text, &span, &inputs, &self.github_identity);
        match resolved {
            Ok(value) => Some(value),
            Err(failure) => {
                let why = match failure {
                    runs_on::Failure::RunTimeContext { name, .. } => {
                        format!("it reads `{name}`, which has no value before the run")
                    }
                    runs_on::Failure::DynamicInput { name, .. } => format!(
                        "it reads input `{name}`, whose value this call site computes at run time"
                    ),
                    runs_on::Failure::Bad { message, .. } => message,
                    runs_on::Failure::NotLabels { got, .. } => {
                        format!("it evaluated to {got}, not text")
                    }
                };
                self.diags.unsupported(
                    "container.expression",
                    span,
                    format!("the {what} cannot be resolved at lowering: {why}"),
                    "a per-scope value resolves at lowering from `inputs` and the checkout's \
                     `github` identity; `matrix` and run-time contexts cannot vary it — use a \
                     literal",
                );
                None
            }
        }
    }

    /// `options:` — raw engine flags, split the way GitHub hands them to the
    /// engine. Opaque from here on: the graph carries them, executors pass
    /// them through.
    fn engine_flags(&mut self, node: Node<'_>, what: &str) -> Vec<SmolStr> {
        let text = node.as_str().unwrap_or_default();
        if text.contains("${{") {
            self.diags.unsupported(
                "container.expression",
                node.span(),
                format!("`{what}` options carry an expression"),
                "options are fixed per scope; use literals",
            );
            return Vec::new();
        }
        crate::split_shell_words(text)
            .into_iter()
            .map(SmolStr::new)
            .collect()
    }

    /// `credentials:` — registry auth. The username resolves at lowering (a
    /// literal, or a static expression); the password must be a whole
    /// `${{ secrets.NAME }}` reference — the graph carries the *name*, and the
    /// executor resolves it inside acquire, so no value ever enters the graph
    /// or the log.
    fn registry_credentials(
        &mut self,
        node: Node<'_>,
        what: &str,
    ) -> Option<ir::RegistryCredentials> {
        let Some(m) = node.as_mapping() else {
            self.diags.error(
                "gha.bad_container",
                node.span(),
                format!("`{what}` credentials need `username` and `password`"),
            );
            return None;
        };
        let mut field = |key: &str| -> Option<(String, Span)> {
            if let Some(n) = m.get(key) {
                n.as_str().map(|s| (s.to_string(), n.span()))
            } else {
                self.diags.error(
                    "gha.bad_container",
                    node.span(),
                    format!("`{what}` credentials need `{key}`"),
                );
                None
            }
        };
        let (username_text, username_span) = field("username")?;
        let (password_text, password_span) = field("password")?;
        let username =
            self.static_scope_text(&username_text, username_span, "registry username")?;
        let Some(password_secret) = exprs::whole_value_secret(&password_text) else {
            self.diags.unsupported(
                "container.credentials",
                password_span,
                format!("`{what}` credentials carry a password that is not a secret reference"),
                "the password must be a whole `${{ secrets.NAME }}` reference; a value never \
                 enters the graph",
            );
            return None;
        };
        Some(ir::RegistryCredentials {
            username:        SmolStr::new(username),
            password_secret: SmolStr::new(password_secret),
        })
    }

    /// `services:` onto the scope: sidecar containers with the job's lifetime,
    /// realized at acquisition. Images and ports are literal; env lowers like
    /// scope env (resolved against the run's parameters); `options` splits into
    /// the flags GitHub hands the engine, opaque from here on.
    fn services_for(&mut self, job: &Job<'a>) -> Vec<ir::ServiceSpec> {
        let Some(node) = job.services else {
            return Vec::new();
        };
        let Some(m) = node.expect_mapping(&mut self.diags, "`services`") else {
            return Vec::new();
        };
        let site = self.base_site(job);
        let mut services = Vec::new();
        for (alias, spec) in m.iter() {
            if alias.is_empty()
                || !alias
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                self.diags.error(
                    "gha.bad_service",
                    spec.span(),
                    format!("service name `{alias}` is not a valid container alias"),
                );
                continue;
            }
            let mut service = ir::ServiceSpec::new(alias, "");
            let image = if spec.is_scalar() {
                Some(spec)
            } else if let Some(sm) = spec.as_mapping() {
                for (key, span) in sm.keys() {
                    match key {
                        "image" | "env" | "ports" | "options" | "credentials" => {}
                        "volumes" => self.diags.unsupported(
                            "services.volumes",
                            span,
                            format!("service `{alias}` mounts volumes"),
                            "service volumes name paths of the runner machine; not yet mapped",
                        ),
                        other => self.diags.error(
                            "gha.bad_service",
                            span,
                            format!("unknown key `{other}` on service `{alias}`"),
                        ),
                    }
                }
                if let Some(node) = sm.get("credentials") {
                    service.credentials = self.registry_credentials(node, "service");
                }
                for (key, value) in sm
                    .get("env")
                    .and_then(|e| e.as_mapping())
                    .iter()
                    .flat_map(Mapping::iter)
                {
                    match self.env_value(&value, &site, exprs::ExprSite::Job) {
                        Some(EnvValue::Plain(v)) => {
                            service.env.insert(SmolStr::new(key), v);
                        }
                        Some(EnvValue::Secret(_)) => self.diags.unsupported(
                            "services.secret_env",
                            value.span(),
                            format!("service `{alias}` env `{key}` is a secret"),
                            "secret-valued service env is not wired yet; use a literal value",
                        ),
                        None => {}
                    }
                }
                if let Some(ports) = sm.get("ports") {
                    let entries: Vec<Node<'_>> = match ports.as_sequence() {
                        Some(seq) => seq.iter().collect(),
                        None => vec![ports],
                    };
                    for entry in entries {
                        let text = super::scalar_text(entry);
                        if text.contains("${{") {
                            self.diags.unsupported(
                                "container.expression",
                                entry.span(),
                                format!("service `{alias}` has an expression-valued port"),
                                "ports are fixed per scope; use literals",
                            );
                            continue;
                        }
                        service.ports.push(SmolStr::new(text));
                    }
                }
                if let Some(options) = sm.get("options") {
                    service.options = self.engine_flags(options, "service");
                }
                sm.get("image")
            } else {
                None
            };
            let Some((text, span)) =
                image.and_then(|i| i.as_str().map(|s| (s.to_string(), i.span())))
            else {
                self.diags.error(
                    "gha.bad_service",
                    spec.span(),
                    format!("service `{alias}` needs an `image`"),
                );
                continue;
            };
            let what = format!("service `{alias}` image");
            let Some(image) = self.static_scope_text(&text, span, &what) else {
                continue;
            };
            service.image = SmolStr::new(image);
            services.push(service);
        }
        services
    }

    /// `runs-on` carrying expressions: resolved now, once per matrix leg, as
    /// GitHub resolves it at queue time with that leg's `matrix` in scope. The
    /// legs come from the same combinators the engine expands with, so the leg
    /// set here and the leg set at run time cannot disagree. Each leg's labels
    /// go through the same placement policy as literal labels — a rejected leg
    /// names itself, and does not stop the others from resolving — and their
    /// union becomes the scope's requirements. Per-leg results are preserved on
    /// the job's `start` node ([`Self::job_shell`]).
    fn expression_runs_on(
        &mut self,
        job: &Job<'a>,
        node: Node<'_>,
        raw: &[runs_on::RawLabel],
        spec: &mut RuntimeSpec,
    ) {
        let inputs = self.placement_inputs();
        let github = self.github_identity.clone();
        let matrix = job.strategy.as_ref().and_then(|s| s.matrix);
        let legs = match matrix {
            // No matrix: the expression still resolves, over an empty `matrix`.
            None => Some(vec![json!({})]),
            Some(matrix) => runs_on::static_legs(matrix, &inputs, &github),
        };
        let Some(legs) = legs else {
            self.diags.unsupported(
                "runs_on.expression",
                node.span(),
                format!(
                    "job `{}`: `runs-on` is an expression and the matrix is not static",
                    job.id
                ),
                "the matrix carries expressions, so its legs — and each leg's `runs-on` — are \
                 unknown before the run; use literal matrix values or a fixed label",
            );
            return;
        };
        let compiled = match runs_on::compile(raw) {
            Ok(compiled) => compiled,
            Err(failure) => {
                self.runs_on_failure(job, failure, None);
                return;
            }
        };
        let mut resolved = Vec::with_capacity(legs.len());
        for leg in &legs {
            match compiled.labels_for(leg, &inputs, &github) {
                Ok(labels) => {
                    let only: Vec<&str> = labels.iter().map(|(l, _)| l.as_str()).collect();
                    let against = matrix.is_some().then_some(leg);
                    for (label, span) in &labels {
                        self.check_label(spec, label, span.clone(), against);
                    }
                    resolved.push(json!({ "leg": leg, "labels": only }));
                }
                // One leg failing to resolve does not stop the others.
                Err(failure) => self.runs_on_failure(job, failure, Some(leg)),
            }
        }
        self.leg_runs_on = Some(Value::Array(resolved));
    }

    /// One label through the placement policy — the same policy for labels
    /// written literally and labels an expression resolved to. Windows and
    /// macOS are specific errors whatever the runner map says; every other
    /// label answers to the map ([`RunnerMap`]), and an unmapped one is an
    /// explicit rejection, never a silently skipped job. Whatever passes joins
    /// the scope's requirements — each label once, so a label shared by many
    /// legs is checked and reported once.
    fn check_label(
        &mut self,
        spec: &mut RuntimeSpec,
        label: &str,
        span: Span,
        leg: Option<&Value>,
    ) {
        if spec.requirements.iter().any(|r| r == label) {
            return;
        }
        // Built only on the diagnostic paths; the common path accepts the label.
        let place = || {
            leg.map(|l| format!(" (matrix leg {l})"))
                .unwrap_or_default()
        };
        match runners::classify(label) {
            LabelClass::Windows => self.diags.unsupported(
                "runs_on.windows",
                span,
                format!("`runs-on: {label}`{}", place()),
                "Windows runners are out of scope; the local executor emulates Linux runners",
            ),
            LabelClass::MacOs => self.diags.unsupported(
                "runs_on.macos",
                span,
                format!("`runs-on: {label}`{}", place()),
                "macOS runners are out of scope; the local executor emulates Linux runners",
            ),
            // Named Linux by its own tokens: the environment the local executor
            // stands in for, config not required.
            LabelClass::Linux => {}
            LabelClass::Opaque => {
                if !self.runners.knows(label) {
                    let configured = self.runners.configured();
                    self.diags.unsupported(
                        "runs_on.unknown",
                        span,
                        format!(
                            "`runs-on: {label}`{} says nothing about its platform",
                            place()
                        ),
                        &format!(
                            "labels naming an Ubuntu or Linux environment place here by their own \
                             text; an opaque pool label joins through the runner map \
                             (`PETRI_RUNNER_LABELS` for the shipped CLI, \
                             `GitHubActions::with_runners` in code){}",
                            if configured.is_empty() {
                                String::new()
                            } else {
                                format!("; configured: {}", configured.join(", "))
                            }
                        ),
                    );
                }
            }
        }
        spec.requirements.push(SmolStr::new(label));
    }

    /// Whether `name` is an input only a caller can provide: this frame is the
    /// file itself (not an inlined callee), `on.workflow_call` declares the
    /// input with no default, and no `workflow_dispatch` declaration offers a
    /// standalone way to run the file with it.
    fn is_callee_only_input(&self, name: &str) -> bool {
        if self.frames[self.current].call.is_some() {
            return false;
        }
        let lowered = name.to_lowercase();
        let declared_defaultless = self.wf.call.as_ref().is_some_and(|interface| {
            interface
                .inputs
                .iter()
                .any(|d| d.name.to_lowercase() == lowered && d.default.is_none())
        });
        let dispatchable = self
            .wf
            .dispatch_inputs
            .iter()
            .any(|d| d.name.to_lowercase() == lowered);
        declared_defaultless && !dispatchable
    }

    /// Why a `runs-on` did not resolve at lowering, as the one
    /// `runs_on.expression` rejection with the specifics in the message.
    fn runs_on_failure(&mut self, job: &Job<'a>, failure: runs_on::Failure, leg: Option<&Value>) {
        let for_leg = leg
            .map(|l| format!(" for matrix leg {l}"))
            .unwrap_or_default();
        match failure {
            runs_on::Failure::RunTimeContext { name, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` reads `{name}`, which has no value before the run{for_leg}",
                    job.id
                ),
                "`runs-on` is resolved at lowering, where `matrix`, `inputs` and the checkout's \
                 `github` identity have values; `needs` is a run-time context — use a fixed \
                 label, matrix values, or an input",
            ),
            runs_on::Failure::DynamicInput { name, span } => {
                // A reusable file, lowered standalone, whose runner input has no
                // default: only a caller can place it, and every call site does
                // (per-call-site resolution above). Its own class, so a corpus
                // count never reads "cannot place by construction" as a gap.
                if self.is_callee_only_input(&name) {
                    self.diags.unsupported(
                        "runs_on.callee_input",
                        span,
                        format!(
                            "job `{}`: `runs-on` reads input `{name}`, which only a caller \
                             provides",
                            job.id
                        ),
                        "this file is reusable: `on.workflow_call` declares the input with no \
                         default, so a standalone lowering has no runner to place — its callers \
                         bind one per call site",
                    );
                } else {
                    self.diags.unsupported(
                        "runs_on.expression",
                        span,
                        format!(
                            "job `{}`: `runs-on` reads input `{name}`, whose value this call \
                             site computes at run time{for_leg}",
                            job.id
                        ),
                        "a `runs-on` input resolves at lowering from a literal `with:` value or \
                         the declared default; pass a literal, or use a fixed label",
                    );
                }
            }
            runs_on::Failure::Bad { message, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` cannot be resolved at lowering{for_leg}: {message}",
                    job.id
                ),
                "`runs-on` is resolved at lowering, per matrix leg; use matrix values, literals, \
                 and the documented functions over them",
            ),
            runs_on::Failure::NotLabels { got, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` evaluated to {got}{for_leg}, not a label or list of labels",
                    job.id
                ),
                "each leg's `runs-on` must resolve to a string or a list of strings",
            ),
        }
    }
}
