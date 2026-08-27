//! A job's scope: `runs-on` placement and `container`, and the environment
//! GitHub gives every step of the job.

use frontend::diag::Span;
use frontend::yaml::Node;
use ir::{ExprOrValue, RuntimeSpec, Scope, ScopeId, Value};
use serde_json::json;
use smol_str::SmolStr;

use crate::model::Job;
use crate::runs_on;

use super::{EnvValue, Lowering};

impl<'w, 'a> Lowering<'w, 'a> {
    pub(super) fn scope_for(&mut self, job: &Job<'a>) -> ScopeId {
        let mut scope = Scope::new(ScopeId::new(0));
        scope.runtime = self.runtime_for(job);

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
        for key in ["os", "arch", "name", "temp", "tool_cache"] {
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

        // Workflow env, then job env on top. Secret refs cannot live in scope env
        // (there is nowhere for them to go but the process), so they are pushed down
        // into every step's env instead; `job_body` reads them back from `site`.
        for (key, node) in self.wf.env.iter().chain(job.env.iter()) {
            match self.env_value(node, &site, false) {
                Some(EnvValue::Plain(v)) => {
                    scope.env.insert(SmolStr::new(key), v);
                }
                Some(EnvValue::Secret(_)) => {
                    // Recorded in job_body via the same lookup; nothing to do here.
                }
                None => {}
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
                    if let Some(label) = runs_on::RawLabel::from_node(node, true) {
                        vec![label]
                    } else if let Some(seq) = node.as_sequence() {
                        seq.iter()
                            .filter_map(|n| runs_on::RawLabel::from_node(n, false))
                            .collect()
                    } else if let Some(m) = node.as_mapping() {
                        // `runs-on: { group: …, labels: … }`
                        let mut out = Vec::new();
                        if let Some(labels) = m.get("labels") {
                            if let Some(one) = runs_on::RawLabel::from_node(labels, false) {
                                out.push(one);
                            } else if let Some(seq) = labels.as_sequence() {
                                out.extend(
                                    seq.iter()
                                        .filter_map(|n| runs_on::RawLabel::from_node(n, false)),
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
            let image = if let Some(s) = container.as_str() {
                Some((s.to_string(), container.span()))
            } else if let Some(m) = container.as_mapping() {
                for (key, span) in m.keys() {
                    match key {
                        "image" | "env" => {}
                        other => self.diags.unsupported(
                            &format!("container.{other}"),
                            span,
                            format!("`container.{other}`"),
                            "only `image` and `env` are mapped onto a container scope; `options` are engine flags, which the graph does not carry",
                        ),
                    }
                }
                m.get("image")
                    .and_then(|i| i.as_str().map(|s| (s.to_string(), i.span())))
            } else {
                None
            };
            match image {
                Some((image, span)) if image.contains("${{") => {
                    self.diags.unsupported(
                        "container.expression",
                        span,
                        "an expression-valued container image",
                        "the image is fixed per scope; use a literal",
                    );
                }
                Some((image, _)) => {
                    let requirements = std::mem::take(&mut spec.requirements);
                    spec = RuntimeSpec::container(&image);
                    spec.requirements = requirements;
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
        match crate::runners::classify(label) {
            crate::runners::LabelClass::Windows => self.diags.unsupported(
                "runs_on.windows",
                span,
                format!("`runs-on: {label}`{}", place()),
                "Windows runners are out of scope; the local executor emulates Linux runners",
            ),
            crate::runners::LabelClass::MacOs => self.diags.unsupported(
                "runs_on.macos",
                span,
                format!("`runs-on: {label}`{}", place()),
                "macOS runners are out of scope; the local executor emulates Linux runners",
            ),
            // Named Linux by its own tokens: the environment the local executor
            // stands in for, config not required.
            crate::runners::LabelClass::Linux => {}
            crate::runners::LabelClass::Opaque => {
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
            runs_on::Failure::DynamicInput { name, span } => self.diags.unsupported(
                "runs_on.expression",
                span,
                format!(
                    "job `{}`: `runs-on` reads input `{name}`, whose value this call site \
                     computes at run time{for_leg}",
                    job.id
                ),
                "a `runs-on` input resolves at lowering from a literal `with:` value or the \
                 declared default; pass a literal, or use a fixed label",
            ),
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
