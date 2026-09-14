//! Model fallback and failover: Fabro's `[run.model.fallbacks]` chains,
//! applied by the LLM steps (readiness item 9a).
//!
//! Three retry mechanisms exist and each has one owner. `lithos-llm` retries
//! a request on the same route through the retry middleware the application
//! puts on the client (`petri::llm_client`). Pebble replays a model turn whose
//! response stream broke, on its default policy. The fallback chain, which
//! moves a stage to another provider or model after a provider-local
//! failure, is planned here and run by Pebble: a native session names the
//! plan's remaining routes to the builder (`fallback_routes`, one
//! `FallbackRoute` per target after the route the plan reached, in order),
//! and Pebble moves the conversation when a model error qualifies and a
//! route remains. The three never share state: Pebble takes a fallback
//! decision only once the client and its own replay have given up on the
//! current route. Petri keeps the engine's node attempts and the
//! output-repair turns. The prompt step (`crate::prompt`) builds no session
//! and runs the same plan on its one-shot requests itself.
//!
//! The chain for a stage is Fabro's: keyed by the canonical id of the
//! requested model ([`resolve`]), filtered against the configured providers,
//! with the requested reasoning effort mapped onto each target through the
//! catalog ([`plan`]). A target with no nearby reasoning level is skipped
//! with [`Notice::NoNearbyReasoningLevel`]; a configured chain with no usable
//! target left emits [`Notice::ChainEmpty`]. The plan is fixed when the stage
//! opens: advancing to a target never activates that target's own chain, and
//! output-repair turns stay on the plan of the original model. A workflow
//! retry (a new attempt) builds a new plan at position 0; it is not a
//! failover.
//!
//! Which errors move the plan is `lithos-llm`'s `failover_eligible`, the rule
//! Pebble applies: a failure the client classifies as retryable, provider
//! authentication, access, not-found, quota, rate-limit, server, network,
//! timeout and stream-decode failures, and a content filter whose provider
//! code is `refusal`. Cancellation, a wall-clock expiry, and any non-LLM
//! agent error end the stage instead. [`ModelFailure::eligible`] carries the
//! same answer on every typed error Petri reports.
//!
//! A native session keeps its conversation across a model change: Pebble
//! resumes the failed session's record on the next route with
//! `ResumeMode::UseModel`, the same session id continuing, and continues the
//! prompt on the history as it stands. A tool the failed turn already ran is
//! not run again: the record carries its result, and the next model answers
//! it with no new input (Pebble's `continue_turn`); a prompt nothing has
//! answered yet is asked again (`replay_prompt`).
//!
//! One fact here is Petri's alone: the plan, with the configuration notices
//! that shaped it. It is the one `StepEvent::Custom` payload this module
//! emits ([`PLAN_EVENT`]), attributed to the node, firing and attempt.
//! Everything Pebble knows about the routes a prompt ran on is on Pebble's
//! own event stream, which the session records as `agent_activity`:
//! `RouteFailover` carries the failed route, the route the prompt continues
//! on, what the prompt spent on the failed route, the typed error and the
//! continuation; `RouteFailoverStopped` carries the route a model error ended
//! the prompt on and why (`ineligible` or `exhausted`); `SessionStarted`
//! names each route's provider and model, `AssistantMessage` each answer's
//! usage, and the prompt report names the route the prompt ended on. Petri
//! restates none of it. The prompt step, which runs no session, reports its
//! own failover on the node's stderr.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error as StdError;
use std::sync::{Arc, Mutex, PoisonError};
use std::{fmt, iter};

use frontend_attractor::fallbacks::ModelRef;
use ir::{Attempt, FiringId, LogStream, StepEvent, Value};
use lithos_llm::Client;
use lithos_llm::catalog::{CatalogModel, CatalogProvider};
use lithos_llm::types::{Error as LlmError, ReasoningEffort, Request, RetryClassification, Speed};
use pebble_coding_agent::{Error as PebbleError, FallbackRoute, InterruptReason};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smol_str::SmolStr;
use steps::{ProgressSender, StepCtx};

use crate::agent::AgentConfig;
use crate::agent::backend::AgentError;
use crate::pebble::speed_of;

/// The `kind` of the payload emitted once per stage when its plan is built:
/// `{ kind, node, firing, attempt, requested: {provider, model}, routes:
/// [{position, provider, model, reasoning_effort, speed}], notices: [{code,
/// level, message}] }`. `routes[0]` is the original route. The only
/// `StepEvent::Custom` kind of this module: the routes a prompt then ran on
/// are Pebble's `RouteFailover` and `RouteFailoverStopped` events.
pub const PLAN_EVENT: &str = "fabro.fallback.plan";

/// One route a stage runs on: a provider and a catalog model id, with the
/// controls the request carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub provider:         String,
    pub model:            String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed:            Option<Speed>,
}

impl Route {
    /// The `provider/model` selector `lithos-llm` resolves without guessing.
    #[must_use]
    pub fn selector(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    fn target(&self) -> Value {
        json!({ "provider": self.provider, "model": self.model })
    }
}

/// A stage's fixed fallback plan: the original route, the usable targets in
/// order, and the position reached. Position 0 is the original.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    original:  Route,
    remaining: Vec<Route>,
    position:  usize,
}

impl Plan {
    /// A plan with no fallback: the route alone.
    #[must_use]
    pub fn single(route: Route) -> Self {
        Self {
            original:  route,
            remaining: Vec::new(),
            position:  0,
        }
    }

    #[must_use]
    pub fn original(&self) -> &Route {
        &self.original
    }

    #[must_use]
    pub fn current(&self) -> &Route {
        self.route_at(self.position)
    }

    /// The route active before the most recent [`Plan::advance`].
    #[must_use]
    pub fn previous(&self) -> &Route {
        self.route_at(self.position.saturating_sub(1))
    }

    fn route_at(&self, position: usize) -> &Route {
        position
            .checked_sub(1)
            .map_or(&self.original, |index| &self.remaining[index])
    }

    #[must_use]
    pub fn position(&self) -> usize {
        self.position
    }

    #[must_use]
    pub fn has_next(&self) -> bool {
        self.position < self.remaining.len()
    }

    /// Move to the next route. `false` when the plan is exhausted.
    pub fn advance(&mut self) -> bool {
        if self.has_next() {
            self.position += 1;
            true
        } else {
            false
        }
    }

    /// Every route, the original first.
    #[must_use]
    pub fn routes(&self) -> Vec<&Route> {
        iter::once(&self.original)
            .chain(self.remaining.iter())
            .collect()
    }

    /// The routes after the one reached, in order: what a native session
    /// names to Pebble as its fallback routes.
    #[must_use]
    pub fn remaining_routes(&self) -> &[Route] {
        &self.remaining[self.position.min(self.remaining.len())..]
    }

    /// The remaining routes as Pebble's, each with its own reasoning effort
    /// and speed and the stage's `max_tokens`.
    pub(crate) fn pebble_routes(&self, max_tokens: Option<i64>) -> Vec<FallbackRoute> {
        self.remaining_routes()
            .iter()
            .map(|route| {
                FallbackRoute::new(route.selector())
                    .with_reasoning_effort(route.reasoning_effort)
                    .with_speed(route.speed)
                    .with_max_tokens(max_tokens)
            })
            .collect()
    }

    fn routes_json(&self) -> Value {
        Value::Array(
            self.routes()
                .into_iter()
                .enumerate()
                .map(|(position, route)| {
                    json!({
                        "position": position,
                        "provider": route.provider,
                        "model": route.model,
                        "reasoning_effort": route.reasoning_effort,
                        "speed": route.speed,
                    })
                })
                .collect(),
        )
    }
}

/// A resolved chain target: a provider and a canonical model id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub provider: String,
    pub model:    String,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.model)
    }
}

/// Why a configured candidate was removed from a chain, or why a chain is
/// unusable. Fabro's `ModelFallbackNotice`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    ProviderUnconfigured {
        requested_model: String,
        reference:       String,
        provider:        String,
    },
    NoConfiguredOffering {
        requested_model: String,
        reference:       String,
        providers:       Vec<String>,
    },
    NoCompatibleModel {
        requested_model: String,
        reference:       String,
        provider:        String,
    },
    /// A selector the catalog does not know on a provider that takes no
    /// passthrough model. Fabro passes it to the provider; Petri's client
    /// cannot address it, so the target is skipped.
    UnknownModel {
        requested_model: String,
        reference:       String,
        provider:        String,
    },
    Duplicate {
        requested_model: String,
        reference:       String,
        target:          Target,
    },
    NoNearbyReasoningLevel {
        requested_model:  String,
        target:           Target,
        requested_effort: ReasoningEffort,
    },
    ChainEmpty {
        requested_model: String,
    },
}

impl Notice {
    /// Fabro's notice code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ChainEmpty { .. } => "model_fallback_chain_empty",
            _ => "model_fallback_skipped",
        }
    }

    /// `info` for a duplicate, `warn` otherwise, as Fabro levels them.
    #[must_use]
    pub fn level(&self) -> &'static str {
        match self {
            Self::Duplicate { .. } => "info",
            _ => "warn",
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::ProviderUnconfigured {
                requested_model,
                reference,
                provider,
            } => format!(
                "Model fallback `{reference}` for requested model `{requested_model}` was skipped \
                 because provider `{provider}` is not configured."
            ),
            Self::NoConfiguredOffering {
                requested_model,
                reference,
                providers,
            } => format!(
                "Model fallback `{reference}` for requested model `{requested_model}` was skipped \
                 because none of its providers are configured. It is offered by: {}.",
                providers.join(", ")
            ),
            Self::NoCompatibleModel {
                requested_model,
                reference,
                provider,
            } => format!(
                "Model fallback `{reference}` for requested model `{requested_model}` was skipped \
                 because provider `{provider}` has no compatible model."
            ),
            Self::UnknownModel {
                requested_model,
                reference,
                provider,
            } => format!(
                "Model fallback `{reference}` for requested model `{requested_model}` was skipped \
                 because provider `{provider}` does not offer that model in the catalog."
            ),
            Self::Duplicate {
                requested_model,
                reference,
                target,
            } => format!(
                "Model fallback `{reference}` for requested model `{requested_model}` was skipped \
                 because target `{target}` already appears in that chain."
            ),
            Self::NoNearbyReasoningLevel {
                requested_model,
                target,
                requested_effort,
            } => format!(
                "Model fallback `{target}` for requested model `{requested_model}` was skipped \
                 because it has no reasoning level near `{}`.",
                effort_name(*requested_effort)
            ),
            Self::ChainEmpty { requested_model } => format!(
                "No usable model fallbacks remain for requested model `{requested_model}` after \
                 filtering its configured candidates."
            ),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "code": self.code(), "level": self.level(), "message": self.message() })
    }
}

fn effort_name(effort: ReasoningEffort) -> String {
    serde_json::to_value(effort)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{effort:?}").to_lowercase())
}

/// The chains of a run, resolved against the catalog: keyed by the canonical
/// id of the requested model, each a list of usable targets in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    chains:  BTreeMap<String, Vec<Target>>,
    notices: Vec<Notice>,
}

impl Resolved {
    #[must_use]
    pub fn chain_for(&self, canonical_model: &str) -> Option<&[Target]> {
        self.chains.get(canonical_model).map(Vec::as_slice)
    }

    /// The notices resolution produced, once per run.
    #[must_use]
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }
}

/// A `[run.model.fallbacks]` table the run cannot use. Fabro refuses these
/// at run start; so does Petri: the `start` stage checks the table through
/// [`check_table`] before anything runs, and a stage that reads the table
/// later meets the same error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "`run.model.fallbacks` key `{key}` names provider `{provider}`; keys must name a \
         requested model"
    )]
    KeyNamesProvider { key: String, provider: String },
    #[error(
        "`run.model.fallbacks` keys name a requested model; use `{selector}` instead of `{key}`"
    )]
    KeyNamesQualifiedModel { key: String, selector: String },
    #[error("`run.model.fallbacks` key `{key}` names no model the catalog knows")]
    KeyUnknown { key: String },
    #[error(
        "`run.model.fallbacks` keys `{previous}` and `{key}` both resolve to requested model \
         `{requested_model}`"
    )]
    KeyConflict {
        previous:        String,
        key:             String,
        requested_model: String,
    },
    #[error(
        "`run.model.fallbacks` entry `{reference}` names provider `{provider}`, which the catalog does not know"
    )]
    UnknownProvider {
        reference: String,
        provider:  String,
    },
    #[error("`run.model.fallbacks` entry `{reference}` does not parse: {message}")]
    BadReference {
        reference: String,
        message:   String,
    },
}

/// Check a `[run.model.fallbacks]` table as the `start` stage carries it
/// (the chains as written, keyed by the requested model): every key and
/// every reference must resolve against the catalog. `Err` is the message
/// the run fails with, class `bad_config`.
pub fn check_table(client: &Client, table: &serde_json::Value) -> Result<(), String> {
    let chains: BTreeMap<String, Vec<String>> = serde_json::from_value(table.clone())
        .map_err(|error| format!("`run.model.fallbacks` does not parse: {error}"))?;
    resolve(client, &chains)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Resolve every chain against the client's catalog and available providers.
/// Fabro's `resolve_model_fallbacks`.
pub fn resolve(
    client: &Client,
    chains: &BTreeMap<String, Vec<String>>,
) -> Result<Resolved, ConfigError> {
    let catalog = client.catalog();
    let is_provider = |name: &str| catalog.provider(name).is_ok();
    let mut resolved = Resolved::default();
    let mut raw_key_by_canonical = HashMap::<String, String>::new();

    for (raw_key, references) in chains {
        let key_ref = raw_key
            .parse::<ModelRef>()
            .map_err(|error| ConfigError::BadReference {
                reference: raw_key.clone(),
                message:   error.to_string(),
            })?
            .qualify(is_provider);
        if let ModelRef::Qualified { selector, .. } = key_ref {
            return Err(ConfigError::KeyNamesQualifiedModel {
                key:      raw_key.clone(),
                selector: selector.clone(),
            });
        }
        if let Ok(provider) = catalog.provider(raw_key) {
            return Err(ConfigError::KeyNamesProvider {
                key:      raw_key.clone(),
                provider: provider.id().to_string(),
            });
        }
        let Some(primary) = canonical_model(client, None, raw_key) else {
            return Err(ConfigError::KeyUnknown {
                key: raw_key.clone(),
            });
        };
        let requested_model = primary.model.clone();
        if let Some(previous) =
            raw_key_by_canonical.insert(requested_model.clone(), raw_key.clone())
        {
            return Err(ConfigError::KeyConflict {
                previous,
                key: raw_key.clone(),
                requested_model,
            });
        }

        let mut targets = Vec::new();
        for reference in references {
            let candidate = resolve_candidate(client, &requested_model, &primary, reference)?;
            let target = match candidate {
                Candidate::Skipped(notice) => {
                    resolved.notices.push(notice);
                    continue;
                }
                Candidate::Target(target) => target,
            };
            if targets.contains(&target) {
                resolved.notices.push(Notice::Duplicate {
                    requested_model: requested_model.clone(),
                    reference: reference.clone(),
                    target,
                });
            } else {
                targets.push(target);
            }
        }
        if targets.is_empty() {
            resolved.notices.push(Notice::ChainEmpty {
                requested_model: requested_model.clone(),
            });
        }
        resolved.chains.insert(requested_model, targets);
    }
    Ok(resolved)
}

enum Candidate {
    Target(Target),
    Skipped(Notice),
}

fn resolve_candidate(
    client: &Client,
    requested_model: &str,
    primary: &Target,
    reference: &str,
) -> Result<Candidate, ConfigError> {
    let catalog = client.catalog();
    let available = client.available_providers();
    let is_provider = |name: &str| catalog.provider(name).is_ok();
    let parsed = reference
        .parse::<ModelRef>()
        .map_err(|error| ConfigError::BadReference {
            reference: reference.to_owned(),
            message:   error.to_string(),
        })?
        .qualify(is_provider);
    let skipped = |notice| Ok(Candidate::Skipped(notice));
    match parsed {
        ModelRef::Bare(token) if is_provider(&token) => {
            let provider = catalog
                .provider(&token)
                .map_err(|_| ConfigError::UnknownProvider {
                    reference: reference.to_owned(),
                    provider:  token.clone(),
                })?;
            let id = provider.id().to_string();
            if !available.contains(provider.id()) {
                return skipped(Notice::ProviderUnconfigured {
                    requested_model: requested_model.to_owned(),
                    reference:       reference.to_owned(),
                    provider:        id,
                });
            }
            // Fabro picks the provider's closest model by feature profile
            // and price. Petri offers the same model id on that provider
            // when the catalog lists it, and skips otherwise.
            match provider.model(&primary.model) {
                Some(model) => Ok(Candidate::Target(Target {
                    provider: id,
                    model:    model.id().to_string(),
                })),
                None => skipped(Notice::NoCompatibleModel {
                    requested_model: requested_model.to_owned(),
                    reference:       reference.to_owned(),
                    provider:        id,
                }),
            }
        }
        ModelRef::Qualified {
            provider: name,
            selector,
        } => {
            let provider = catalog
                .provider(&name)
                .map_err(|_| ConfigError::UnknownProvider {
                    reference: reference.to_owned(),
                    provider:  name.clone(),
                })?;
            let id = provider.id().to_string();
            if !available.contains(provider.id()) {
                return skipped(Notice::ProviderUnconfigured {
                    requested_model: requested_model.to_owned(),
                    reference:       reference.to_owned(),
                    provider:        id,
                });
            }
            match provider.model(&selector) {
                Some(model) => Ok(Candidate::Target(Target {
                    provider: id,
                    model:    model.id().to_string(),
                })),
                None if provider.allows_passthrough() => Ok(Candidate::Target(Target {
                    provider: id,
                    model:    selector,
                })),
                None => skipped(Notice::UnknownModel {
                    requested_model: requested_model.to_owned(),
                    reference:       reference.to_owned(),
                    provider:        id,
                }),
            }
        }
        ModelRef::Bare(selector) => {
            if let Some(target) = route_of(client, &selector) {
                return Ok(Candidate::Target(target));
            }
            let offering: Vec<String> = catalog
                .providers()
                .filter(|provider| provider.model(&selector).is_some())
                .map(|provider| provider.id().to_string())
                .collect();
            if !offering.is_empty() {
                return skipped(Notice::NoConfiguredOffering {
                    requested_model: requested_model.to_owned(),
                    reference:       reference.to_owned(),
                    providers:       offering,
                });
            }
            let passthrough = catalog
                .provider(&primary.provider)
                .is_ok_and(CatalogProvider::allows_passthrough);
            if passthrough {
                return Ok(Candidate::Target(Target {
                    provider: primary.provider.clone(),
                    model:    selector,
                }));
            }
            skipped(Notice::UnknownModel {
                requested_model: requested_model.to_owned(),
                reference:       reference.to_owned(),
                provider:        primary.provider.clone(),
            })
        }
    }
}

/// The route the client resolves `selector` to, on an available provider.
fn route_of(client: &Client, selector: &str) -> Option<Target> {
    let probe = Request::builder()
        .model(selector)
        .user("probe")
        .build()
        .ok()?;
    let route = client.resolve_route(&probe).ok()?;
    Some(Target {
        provider: route.provider().id().to_string(),
        model:    route.model().id().to_string(),
    })
}

/// The canonical provider and model id of a request: what the client would
/// route to, else what any catalog provider lists under the selector.
/// The model a node runs on when it names none but its provider is known:
/// the provider's default model in the catalog, as Fabro's `--provider`
/// alone picks the provider's default offering. `Err` says why there is
/// none.
pub(crate) fn provider_default_model(client: &Client, provider: &str) -> Result<String, String> {
    let catalog = client.catalog();
    let Ok(row) = catalog.provider(provider) else {
        return Err(format!(
            "provider `{provider}` is not in the catalog; set `model`, the graph's \
             `default_model`, `[run.model] name` in workflow.toml, or `--model` at launch"
        ));
    };
    row.default_model().map(str::to_owned).ok_or_else(|| {
        format!(
            "provider `{provider}` names no default model in the catalog; set `model`, the \
             graph's `default_model`, `[run.model] name` in workflow.toml, or `--model` at launch"
        )
    })
}

/// Fill a node's model from its provider's catalog default when the node,
/// the graph, the run configuration and the launch named a provider but no
/// model. A node with a model, or with neither, is left as it is.
pub(crate) fn fill_provider_default(
    client: &Client,
    model: &mut Option<String>,
    provider: Option<&str>,
) -> Result<(), String> {
    if model.as_deref().is_none_or(|s| s.trim().is_empty())
        && let Some(provider) = provider
    {
        *model = Some(provider_default_model(client, provider)?);
    }
    Ok(())
}

fn canonical_model(client: &Client, provider: Option<&str>, model: &str) -> Option<Target> {
    let selector = match provider {
        Some(provider) if !model.starts_with(&format!("{provider}/")) => {
            format!("{provider}/{model}")
        }
        _ => model.to_owned(),
    };
    if let Some(target) = route_of(client, &selector) {
        return Some(target);
    }
    let catalog = client.catalog();
    if let Some(provider) = provider {
        let provider = catalog.provider(provider).ok()?;
        return provider.model(model).map(|m| Target {
            provider: provider.id().to_string(),
            model:    m.id().to_string(),
        });
    }
    catalog.providers().find_map(|provider| {
        provider.model(model).map(|m| Target {
            provider: provider.id().to_string(),
            model:    m.id().to_string(),
        })
    })
}

/// What a stage asks for: its model, provider and request controls as
/// configured.
#[derive(Clone, Debug, Default)]
pub struct Requested<'a> {
    pub provider:         Option<&'a str>,
    pub model:            &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub speed:            Option<Speed>,
}

/// A stage's plan and the notices its construction produced.
#[derive(Clone, Debug)]
pub struct Planned {
    pub plan:    Plan,
    pub notices: Vec<Notice>,
}

/// Why a stage has no plan.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("no available provider offers `{selector}`")]
    Primary { selector: String },
}

/// Build a stage's plan. Fabro's `fallback_plan`: the original route on the
/// canonical model, then the chain keyed by that model with the original
/// filtered out and each target's controls mapped through the catalog.
pub fn plan(
    client: &Client,
    resolved: &Resolved,
    requested: &Requested<'_>,
) -> Result<Planned, PlanError> {
    let primary =
        canonical_model(client, requested.provider, requested.model).ok_or_else(|| {
            PlanError::Primary {
                selector: requested.provider.map_or_else(
                    || requested.model.to_owned(),
                    |p| format!("{p}/{}", requested.model),
                ),
            }
        })?;
    let original = Route {
        provider:         primary.provider.clone(),
        model:            primary.model.clone(),
        reasoning_effort: requested.reasoning_effort,
        speed:            requested.speed,
    };
    let Some(configured) = resolved.chain_for(&primary.model) else {
        return Ok(Planned {
            plan:    Plan::single(original),
            notices: Vec::new(),
        });
    };
    let mut remaining = Vec::new();
    let mut notices = Vec::new();
    for target in configured {
        if target.provider == original.provider && target.model == original.model {
            continue;
        }
        let model = client
            .catalog()
            .provider(&target.provider)
            .ok()
            .and_then(|provider| provider.model(&target.model));
        let effort = match (requested.reasoning_effort, model) {
            (None, _) | (Some(_), None) => requested.reasoning_effort,
            (Some(effort), Some(model)) => match closest_supported(effort, model) {
                Effort::Keep => Some(effort),
                Effort::Level(level) => Some(level),
                Effort::None => {
                    notices.push(Notice::NoNearbyReasoningLevel {
                        requested_model:  original.model.clone(),
                        target:           target.clone(),
                        requested_effort: effort,
                    });
                    continue;
                }
            },
        };
        remaining.push(Route {
            provider:         target.provider.clone(),
            model:            target.model.clone(),
            reasoning_effort: effort,
            speed:            requested.speed,
        });
    }
    if !configured.is_empty() && remaining.is_empty() {
        notices.push(Notice::ChainEmpty {
            requested_model: original.model.clone(),
        });
    }
    Ok(Planned {
        plan: Plan {
            original,
            remaining,
            position: 0,
        },
        notices,
    })
}

enum Effort {
    /// The catalog says nothing about the model's levels: keep the request.
    Keep,
    Level(ReasoningEffort),
    /// The model advertises levels and none is near the request.
    None,
}

const LEVELS: [ReasoningEffort; 6] = [
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
    ReasoningEffort::Max,
];

fn rank(effort: ReasoningEffort) -> u8 {
    LEVELS
        .iter()
        .position(|level| *level == effort)
        .and_then(|index| u8::try_from(index).ok())
        .unwrap_or(u8::MAX)
}

/// Fabro's `closest_supported`: the nearest advertised level, a tie going
/// to the higher one. A model that advertises nothing (every level unknown)
/// keeps the request for the provider to judge.
fn closest_supported(requested: ReasoningEffort, model: &CatalogModel) -> Effort {
    let capabilities = model.capabilities();
    let supports = |level: ReasoningEffort| capabilities.reasoning_effort(level);
    if model.is_passthrough()
        || LEVELS
            .iter()
            .all(|level| !supports(*level).is_supported() && !supports(*level).is_unsupported())
    {
        return Effort::Keep;
    }
    LEVELS
        .iter()
        .copied()
        .filter(|level| supports(*level).is_supported())
        .min_by_key(|level| {
            (
                rank(requested).abs_diff(rank(*level)),
                Reverse(rank(*level)),
            )
        })
        .map_or(Effort::None, Effort::Level)
}

/// A typed model failure, preserved across the adapter so events can carry
/// it and the prompt step's own plan can read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFailure {
    pub kind:          String,
    pub message:       String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider:      Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status:        Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    /// The client's own retry classification: `never`, `safe`, or `after`.
    pub retry:         String,
    /// Whether the failure may move a plan: `lithos-llm`'s
    /// `failover_eligible`, the rule Pebble applies.
    pub eligible:      bool,
}

fn retry_name(retry: RetryClassification) -> &'static str {
    match retry {
        RetryClassification::Never => "never",
        RetryClassification::Safe => "safe",
        RetryClassification::After { .. } => "after",
        _ => "unknown",
    }
}

impl ModelFailure {
    #[must_use]
    pub fn from_error(error: &LlmError) -> Self {
        Self {
            kind:          error.kind().as_str().to_owned(),
            message:       error.message().to_owned(),
            provider:      error.provider().map(ToString::to_string),
            status:        error.status(),
            provider_code: error.provider_code().map(str::to_owned),
            retry:         retry_name(error.retry_classification()).to_owned(),
            eligible:      error.failover_eligible(),
        }
    }

    /// The failure class a stage reports: `llm:<kind>`.
    #[must_use]
    pub fn class(&self) -> String {
        format!("llm:{}", self.kind)
    }
}

impl fmt::Display for ModelFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model request failed ({}): {}", self.kind, self.message)?;
        if let Some(provider) = &self.provider {
            write!(f, " [provider {provider}")?;
            if let Some(status) = self.status {
                write!(f, ", status {status}")?;
            }
            if let Some(code) = &self.provider_code {
                write!(f, ", code {code}")?;
            }
            write!(f, "]")?;
        }
        Ok(())
    }
}

/// How a Pebble prompt error is treated.
#[derive(Debug)]
pub enum Disposition {
    /// The session was cancelled: never a failover.
    Cancelled,
    /// A model error, eligible or not.
    Model(ModelFailure),
    /// A non-LLM agent error: never a failover. The class and message a
    /// stage reports.
    Other { class: String, message: String },
}

/// Classify a Pebble prompt error. Fabro's `classify_agent_error`, on
/// Pebble's error type.
#[must_use]
pub fn classify(error: &PebbleError) -> Disposition {
    match error {
        PebbleError::Interrupted(InterruptReason::Cancelled) => Disposition::Cancelled,
        PebbleError::Interrupted(reason) => Disposition::Other {
            class:   "agent_interrupted".into(),
            message: format!("the agent session was interrupted: {reason}"),
        },
        PebbleError::Llm(llm) => Disposition::Model(ModelFailure::from_error(llm)),
        PebbleError::Compaction(pebble_coding_agent::CompactionError::Llm(llm)) => {
            Disposition::Model(ModelFailure::from_error(llm))
        }
        PebbleError::ToolExecution(message) => Disposition::Other {
            class:   "tool_execution".into(),
            message: format!("tool execution error: {message}"),
        },
        other => Disposition::Other {
            class:   "pebble_prompt".into(),
            message: error_chain(other),
        },
    }
}

fn error_chain(error: &dyn StdError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The run's resolved chains and the notices already reported, so a
/// configuration warning is said once per run, as Fabro says it. Registered
/// as a run service by [`crate::register`].
#[derive(Default)]
pub struct FallbackService {
    resolved: Mutex<HashMap<String, Arc<Result<Resolved, ConfigError>>>>,
    noticed:  Mutex<HashSet<String>>,
}

impl FallbackService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The chains resolved against `client`, once per distinct table.
    pub fn resolve(
        &self,
        client: &Client,
        chains: &BTreeMap<String, Vec<String>>,
    ) -> Arc<Result<Resolved, ConfigError>> {
        let key = serde_json::to_string(chains).unwrap_or_default();
        let mut cache = self.resolved.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(resolve(client, chains)))
            .clone()
    }

    /// Whether `notice` is new for this run.
    pub fn first(&self, notice: &Notice) -> bool {
        self.noticed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(notice.message())
    }
}

/// The stage identity the plan event carries.
pub(crate) struct Stage {
    logs:    ProgressSender,
    node:    SmolStr,
    firing:  FiringId,
    attempt: Attempt,
}

impl Stage {
    pub(crate) fn of(ctx: &StepCtx) -> Self {
        Self {
            logs:    ctx.logs.clone(),
            node:    ctx.node.clone(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
        }
    }

    /// The stage's plan and the notices that shaped it, as [`PLAN_EVENT`].
    pub(crate) async fn plan(&self, plan: &Plan, notices: &[Notice]) {
        let _ = self
            .logs
            .send(StepEvent::Custom(json!({
                "kind": PLAN_EVENT,
                "node": self.node,
                "firing": self.firing,
                "attempt": self.attempt,
                "requested": plan.original.target(),
                "routes": plan.routes_json(),
                "notices": notices.iter().map(Notice::to_json).collect::<Vec<_>>(),
            })))
            .await;
    }
}

/// Build a native agent node's plan from its config, reporting each new
/// notice on stderr once per run.
pub(crate) async fn plan_for_agent(
    config: &AgentConfig,
    ctx: &mut StepCtx,
    client: &Client,
) -> Result<Planned, AgentError> {
    let model = config
        .model
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            AgentError::failed("bad_config", "backend=api requires model or default_model")
        })?;
    let reasoning_effort = config
        .reasoning_effort
        .as_ref()
        .map(|value| serde_json::from_value::<ReasoningEffort>(json!(value)))
        .transpose()
        .map_err(|e| AgentError::failed("bad_config", e.to_string()))?;
    let speed = match config.speed.as_deref() {
        None => None,
        Some(text) => Some(speed_of(text).ok_or_else(|| {
            AgentError::failed(
                "bad_config",
                format!(
                    "Invalid speed \"{text}\" for node \"{}\"; expected one of: standard, fast",
                    config.node
                ),
            )
        })?),
    };
    let requested = Requested {
        provider: config.provider.as_deref(),
        model,
        reasoning_effort,
        speed,
    };
    plan_for(ctx, client, &config.fallbacks, &requested)
        .await
        .map_err(|error| match error {
            PlanError::Config(error) => AgentError::failed("bad_config", error.to_string()),
            PlanError::Primary { .. } => {
                AgentError::failed("llm:model_selection", error.to_string())
            }
        })
}

/// Build a plan through the run's [`FallbackService`], reporting each new
/// notice on stderr once per run. The notices in the result are the ones
/// this stage produced, new or not, for its plan event.
pub(crate) async fn plan_for(
    ctx: &mut StepCtx,
    client: &Client,
    chains: &BTreeMap<String, Vec<String>>,
    requested: &Requested<'_>,
) -> Result<Planned, PlanError> {
    let service = ctx.capability::<FallbackService>();
    let resolved = match &service {
        Some(service) => service.resolve(client, chains),
        None => Arc::new(resolve(client, chains)),
    };
    let resolved = match resolved.as_ref() {
        Ok(resolved) => resolved,
        Err(error) => return Err(PlanError::Config(error.clone())),
    };
    let planned = plan(client, resolved, requested)?;
    let mut notices = resolved.notices().to_vec();
    notices.extend(planned.notices.iter().cloned());
    for notice in &notices {
        let first = service.as_ref().is_none_or(|service| service.first(notice));
        if first {
            ctx.log(
                LogStream::Stderr,
                format!("{}: {}", notice.level(), notice.message()),
            )
            .await;
        }
    }
    Ok(Planned {
        plan: planned.plan,
        notices,
    })
}

#[cfg(test)]
mod tests {
    use lithos_llm::catalog::Catalog;
    use lithos_llm::types::ErrorKind;

    use super::*;

    const CATALOG: &str = r#"
        schema_version = 1

        [providers.alpha]
        display_name = "Alpha"
        adapter = "openai"
        codec = "openai-responses"
        base_url = "http://127.0.0.1:1"
        default_model = "one"
        auth = { type = "none" }

        [providers.alpha.models.one]
        display_name = "One"
        aliases = ["uno"]
        api_model = "one-v1"
        capabilities = { text = true, tools = true, reasoning = true, reasoning_effort = { minimal = false, low = true, medium = true, high = true, xhigh = false, max = false } }

        [providers.alpha.models.two]
        display_name = "Two"
        api_model = "two-v1"
        capabilities = { text = true, tools = true, reasoning = false, reasoning_effort = { minimal = false, low = false, medium = false, high = false, xhigh = false, max = false } }

        [providers.beta]
        display_name = "Beta"
        adapter = "openai"
        codec = "openai-responses"
        base_url = "http://127.0.0.1:2"
        default_model = "one"
        auth = { type = "none" }

        [providers.beta.models.one]
        display_name = "One on Beta"
        api_model = "beta-one"
        capabilities = { text = true, tools = true, reasoning = true, reasoning_effort = { minimal = false, low = true, medium = false, high = false, xhigh = true, max = true } }

        [providers.beta.models.three]
        display_name = "Three"
        api_model = "three"
        capabilities = { text = true, tools = true, reasoning = true }

        [providers.gamma]
        display_name = "Gamma"
        adapter = "openai"
        codec = "openai-responses"
        base_url = "http://127.0.0.1:3"
        default_model = "one"
        auth = { type = "none" }

        [providers.gamma.models.one]
        display_name = "One on Gamma"
        api_model = "gamma-one"
        capabilities = { text = true, tools = true, reasoning = true }
    "#;

    fn client(enabled: &[&str]) -> Client {
        let catalog = Catalog::builder()
            .toml_layer("test", CATALOG)
            .expect("layer")
            .build()
            .expect("catalog");
        let build = Client::builder()
            .catalog(catalog)
            .enabled_providers(enabled.iter().map(|p| (*p).to_owned()))
            .build()
            .expect("client");
        assert!(build.issues.is_empty(), "{:?}", build.issues);
        build.client
    }

    fn chains(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(key, refs)| {
                (
                    (*key).to_owned(),
                    refs.iter().map(|r| (*r).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn target(provider: &str, model: &str) -> Target {
        Target {
            provider: provider.into(),
            model:    model.into(),
        }
    }

    #[test]
    fn chains_are_keyed_by_the_canonical_model_and_stay_independent() {
        let client = client(&["alpha", "beta"]);
        let resolved = resolve(
            &client,
            &chains(&[
                ("uno", &["beta:one", "beta", "alpha/two"]),
                ("three", &["alpha:one"]),
            ]),
        )
        .expect("resolves");
        assert_eq!(
            resolved.chain_for("one"),
            Some([target("beta", "one"), target("alpha", "two"),].as_slice()),
            "{resolved:?}"
        );
        // `beta` alone offers the same model id on beta: a duplicate.
        assert!(matches!(
            &resolved.notices()[0],
            Notice::Duplicate { reference, .. } if reference == "beta"
        ));
        assert_eq!(
            resolved.chain_for("three"),
            Some([target("alpha", "one")].as_slice())
        );
    }

    #[test]
    fn keys_that_name_a_provider_or_a_qualified_model_are_refused() {
        let client = client(&["alpha", "beta"]);
        assert!(matches!(
            resolve(&client, &chains(&[("alpha", &["beta:one"])])),
            Err(ConfigError::KeyNamesProvider { .. })
        ));
        assert!(matches!(
            resolve(&client, &chains(&[("alpha:one", &["beta:one"])])),
            Err(ConfigError::KeyNamesQualifiedModel { ref selector, .. }) if selector == "one"
        ));
        assert!(matches!(
            resolve(
                &client,
                &chains(&[("one", &["beta:one"]), ("uno", &["beta:three"])])
            ),
            Err(ConfigError::KeyConflict { .. })
        ));
        assert!(matches!(
            resolve(&client, &chains(&[("nope", &["beta:one"])])),
            Err(ConfigError::KeyUnknown { .. })
        ));
        // Only the legacy `provider/model` form names a provider outright; a
        // `delta:one` token whose prefix is no provider stays a bare selector
        // that no provider offers.
        assert!(matches!(
            resolve(&client, &chains(&[("one", &["delta/one"])])),
            Err(ConfigError::UnknownProvider { .. })
        ));
        let resolved = resolve(&client, &chains(&[("one", &["delta:one"])])).expect("resolves");
        assert!(matches!(
            resolved.notices().first(),
            Some(Notice::UnknownModel { .. })
        ));
    }

    #[test]
    fn unconfigured_and_unknown_candidates_are_skipped_with_notices() {
        let client = client(&["alpha"]);
        let resolved = resolve(
            &client,
            &chains(&[("one", &["beta:one", "gamma", "three", "alpha:nine"])]),
        )
        .expect("resolves");
        assert_eq!(resolved.chain_for("one"), Some([].as_slice()));
        let codes: Vec<&str> = resolved
            .notices()
            .iter()
            .map(|n| match n {
                Notice::ProviderUnconfigured { .. } => "unconfigured",
                Notice::NoConfiguredOffering { .. } => "no_offering",
                Notice::UnknownModel { .. } => "unknown",
                Notice::ChainEmpty { .. } => "empty",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(codes, [
            "unconfigured",
            "unconfigured",
            "no_offering",
            "unknown",
            "empty"
        ]);
        assert!(resolved.notices().iter().all(|n| n.level() == "warn"));
        assert_eq!(resolved.notices()[4].code(), "model_fallback_chain_empty");
    }

    #[test]
    fn a_plan_maps_effort_per_target_and_skips_targets_with_no_nearby_level() {
        let client = client(&["alpha", "beta"]);
        let resolved = resolve(
            &client,
            &chains(&[("one", &["beta:one", "alpha:two", "beta:three", "alpha:uno"])]),
        )
        .expect("resolves");
        let planned = plan(&client, &resolved, &Requested {
            provider:         Some("alpha"),
            model:            "uno",
            reasoning_effort: Some(ReasoningEffort::Medium),
            speed:            Some(Speed::Fast),
        })
        .expect("plan");
        let routes = planned.plan.routes();
        assert_eq!(routes[0].selector(), "alpha/one");
        assert_eq!(routes[0].reasoning_effort, Some(ReasoningEffort::Medium));
        // beta/one has low and xhigh: medium is nearer to low (1) than xhigh (2).
        assert_eq!(routes[1].selector(), "beta/one");
        assert_eq!(routes[1].reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(routes[1].speed, Some(Speed::Fast));
        // beta/three advertises no levels: the request is kept.
        assert_eq!(routes[2].selector(), "beta/three");
        assert_eq!(routes[2].reasoning_effort, Some(ReasoningEffort::Medium));
        assert_eq!(routes.len(), 3, "{routes:?}");
        assert!(matches!(
            planned.notices.as_slice(),
            [Notice::NoNearbyReasoningLevel { target, requested_effort: ReasoningEffort::Medium, .. }]
                if target.model == "two"
        ));
        // The original route is filtered out of its own chain (`alpha:uno`).
        assert!(
            !routes
                .iter()
                .any(|r| r.provider == "alpha" && r.model == "one" && r.reasoning_effort.is_none())
        );
    }

    #[test]
    fn a_plan_whose_chain_has_no_usable_target_reports_chain_empty() {
        let client = client(&["alpha"]);
        let resolved = resolve(&client, &chains(&[("one", &["alpha:two"])])).expect("resolves");
        let planned = plan(&client, &resolved, &Requested {
            provider:         None,
            model:            "one",
            reasoning_effort: Some(ReasoningEffort::High),
            speed:            None,
        })
        .expect("plan");
        assert!(!planned.plan.has_next());
        assert!(matches!(planned.notices.as_slice(), [
            Notice::NoNearbyReasoningLevel { .. },
            Notice::ChainEmpty { .. }
        ]));
        // Without an effort the same target is usable.
        let planned = plan(&client, &resolved, &Requested {
            provider:         None,
            model:            "one",
            reasoning_effort: None,
            speed:            None,
        })
        .expect("plan");
        assert!(planned.plan.has_next());
        assert!(planned.notices.is_empty());
    }

    #[test]
    fn a_target_ties_round_up_and_a_model_with_no_chain_has_a_single_route() {
        let client = client(&["alpha", "beta"]);
        let resolved = resolve(&client, &chains(&[("three", &["beta:one"])])).expect("resolves");
        // beta/one: low (1) and xhigh (4); high (3) is 2 from low and 1 from xhigh.
        let planned = plan(&client, &resolved, &Requested {
            provider:         Some("beta"),
            model:            "three",
            reasoning_effort: Some(ReasoningEffort::High),
            speed:            None,
        })
        .expect("plan");
        assert_eq!(
            planned.plan.routes()[1].reasoning_effort,
            Some(ReasoningEffort::Xhigh)
        );
        // Equal distance: max (5) vs low (1) from medium... use beta/one with medium:
        // low=1, xhigh=2.
        let planned = plan(&client, &resolved, &Requested {
            provider:         Some("beta"),
            model:            "three",
            reasoning_effort: Some(ReasoningEffort::Max),
            speed:            None,
        })
        .expect("plan");
        assert_eq!(
            planned.plan.routes()[1].reasoning_effort,
            Some(ReasoningEffort::Max)
        );
        let single = plan(&client, &resolved, &Requested {
            provider:         Some("alpha"),
            model:            "two",
            reasoning_effort: None,
            speed:            None,
        })
        .expect("plan");
        assert_eq!(single.plan.routes().len(), 1);
        assert!(single.notices.is_empty());
        assert!(matches!(
            plan(&client, &resolved, &Requested {
                provider: None,
                model: "missing",
                ..Requested::default()
            }),
            Err(PlanError::Primary { .. })
        ));
    }

    #[test]
    fn a_plan_advances_through_its_routes_once() {
        let mut plan = Plan {
            original:  Route {
                provider:         "a".into(),
                model:            "m".into(),
                reasoning_effort: None,
                speed:            None,
            },
            remaining: vec![Route {
                provider:         "b".into(),
                model:            "m".into(),
                reasoning_effort: None,
                speed:            None,
            }],
            position:  0,
        };
        assert_eq!(plan.current().selector(), "a/m");
        assert!(plan.advance());
        assert_eq!(plan.current().selector(), "b/m");
        assert_eq!(plan.previous().selector(), "a/m");
        assert!(!plan.advance());
        assert_eq!(plan.position(), 1);
        let json = serde_json::to_string(&plan).expect("json");
        let back: Plan = serde_json::from_str(&json).expect("plan");
        assert_eq!(back, plan);
    }

    /// The typed failure carries `lithos-llm`'s `failover_eligible`, the
    /// rule Pebble moves a plan on: the provider-local kinds, a refusal, and
    /// anything the client would retry.
    #[test]
    fn eligibility_is_the_clients_failover_rule() {
        let error = |kind: ErrorKind| LlmError::new(kind, "test");
        let eligible = |error: &LlmError| ModelFailure::from_error(error).eligible;
        for kind in [
            ErrorKind::RateLimit,
            ErrorKind::Server,
            ErrorKind::Network,
            ErrorKind::StreamDecode,
            ErrorKind::Authentication,
            ErrorKind::AccessDenied,
            ErrorKind::NotFound,
            ErrorKind::QuotaExceeded,
            ErrorKind::Timeout,
        ] {
            assert!(eligible(&error(kind.clone())), "{kind:?} is eligible");
        }
        for kind in [
            ErrorKind::InvalidRequest,
            ErrorKind::ContextLength,
            ErrorKind::Configuration,
            ErrorKind::ModelSelection,
            ErrorKind::ResourceLimit,
            ErrorKind::Middleware,
            ErrorKind::Cancelled,
            ErrorKind::ContentFilter,
            ErrorKind::Provider,
            ErrorKind::ResponseDecode,
            ErrorKind::Unknown("newer".into()),
        ] {
            assert!(!eligible(&error(kind.clone())), "{kind:?} is not eligible");
        }
        // A failure the client would retry qualifies whatever its kind.
        assert!(eligible(
            &error(ErrorKind::Provider).with_retry(RetryClassification::Safe)
        ));
        let refusal = error(ErrorKind::ContentFilter).with_provider_code("refusal");
        assert!(eligible(&refusal));
        let filtered = error(ErrorKind::ContentFilter).with_provider_code("content_filter");
        assert!(!eligible(&filtered));
        let failure = ModelFailure::from_error(
            &error(ErrorKind::RateLimit)
                .with_status(429)
                .with_retry(RetryClassification::Safe),
        );
        assert_eq!(failure.class(), "llm:rate_limit");
        assert_eq!(failure.retry, "safe");
        assert_eq!(failure.status, Some(429));
        assert!(failure.eligible);
    }

    #[test]
    fn pebble_errors_classify_as_the_reference_classifies_agent_errors() {
        assert!(matches!(
            classify(&PebbleError::Interrupted(InterruptReason::Cancelled)),
            Disposition::Cancelled
        ));
        assert!(matches!(
            classify(&PebbleError::Interrupted(InterruptReason::WallClockTimeout)),
            Disposition::Other { ref class, .. } if class == "agent_interrupted"
        ));
        assert!(matches!(
            classify(&PebbleError::Llm(LlmError::new(ErrorKind::Server, "boom"))),
            Disposition::Model(ModelFailure { eligible: true, .. })
        ));
        assert!(matches!(
            classify(&PebbleError::ToolExecution("rm failed".into())),
            Disposition::Other { ref class, .. } if class == "tool_execution"
        ));
        assert!(matches!(
            classify(&PebbleError::SessionClosed),
            Disposition::Other { ref class, .. } if class == "pebble_prompt"
        ));
    }
}
