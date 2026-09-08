//! The comparison rules of black box phase 5, as code.
//!
//! Both engines are projected onto one semantic shape ([`Projection`]): the
//! terminal status, the main-line stage path in causal order, each fork's
//! branch results in dispatch order with the causal stage order inside each
//! branch, the final workflow-owned context, the declared artifacts, the
//! interviews, the provider requests, and side-effect counts.
//!
//! Normalization touches only named incidental differences (generated run
//! ids, absolute paths, blob references) through an explicit identity map
//! the projection carries. Nothing sorts arrays, coerces types, drops nulls,
//! drops context updates, or collapses a failed stage. Engine bookkeeping
//! keys are removed from the compared context only when a scenario names
//! them, and the removed keys stay in the projection beside the compared
//! ones.
//!
//! [`compare`] emits one named [`Difference`] per disagreement. A
//! difference is accepted only by a committed decision record
//! (`crates/fabro/acceptance/decisions/*.toml`) whose `accepts` list names
//! the difference kind and whose scope names the scenario.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::{fs, iter, mem};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::fabro_adapter::{FabroRun, repo_root};
use super::inspect;
use super::launch::Finished;
use super::twins::{Provider, Twin};

/// What a scenario declares about its comparison, written before any run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Rules {
    /// Context keys that are engine bookkeeping, listed one by one. A key
    /// ending in `.` names a prefix. They are removed from the compared
    /// context and kept under `bookkeeping`.
    pub(crate) bookkeeping: Vec<String>,
    /// Workspace files whose content is compared, relative to the
    /// workspace.
    pub(crate) artifacts:   Vec<String>,
}

impl Rules {
    fn is_bookkeeping(&self, key: &str) -> bool {
        self.bookkeeping.iter().any(|rule| {
            if let Some(prefix) = rule.strip_suffix('.') {
                key == prefix || key.starts_with(rule) && !prefix.is_empty()
            } else {
                key == rule
            }
        })
    }
}

/// One stage record: the node and its final outcome, in the shared
/// vocabulary `succeeded`, `partially_succeeded`, `failed`, `skipped`,
/// `cancelled`, `timed_out`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stage {
    pub(crate) node:    String,
    pub(crate) outcome: String,
}

/// One branch of a fork: the result envelope the join saw and the causal
/// stage order inside the branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Branch {
    pub(crate) id:              String,
    pub(crate) index:           Option<u64>,
    pub(crate) item_label:      Option<String>,
    pub(crate) status:          String,
    pub(crate) context_updates: Value,
    pub(crate) stages:          Vec<Stage>,
}

/// One fork: its node and its branches in dispatch order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Fork {
    pub(crate) node:     String,
    pub(crate) branches: Vec<Branch>,
}

/// One interview: what was asked and what was answered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Interview {
    pub(crate) node:     String,
    pub(crate) kind:     String,
    pub(crate) text:     String,
    pub(crate) options:  Vec<String>,
    pub(crate) reply:    Value,
    pub(crate) delivery: String,
}

/// One provider request at the twin boundary, projected to what both
/// engines must agree on: the provider, model, reasoning effort, the tool
/// calls the reply asked for, and the scenario the twin matched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub(crate) provider: String,
    pub(crate) model:    String,
    pub(crate) effort:   Option<String>,
    pub(crate) scenario: Option<String>,
}

/// The semantic projection of one run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Projection {
    pub(crate) engine:            String,
    pub(crate) status:            String,
    pub(crate) path:              Vec<Stage>,
    pub(crate) forks:             Vec<Fork>,
    pub(crate) context:           BTreeMap<String, Value>,
    pub(crate) bookkeeping:       BTreeMap<String, Value>,
    pub(crate) artifacts:         BTreeMap<String, Option<String>>,
    pub(crate) interviews:        Vec<Interview>,
    /// The workflow's own model requests, in arrival order.
    pub(crate) requests:          Vec<Request>,
    /// Requests a platform makes outside any stage (Fabro's run-title
    /// call), listed apart from the workflow's and counted under
    /// `counts.platform_requests`. Named by [`platform_request`].
    #[serde(default)]
    pub(crate) platform_requests: Vec<Request>,
    pub(crate) counts:            BTreeMap<String, u64>,
    /// The identity mapping applied: placeholder to the raw value.
    pub(crate) identities:        BTreeMap<String, String>,
}

/// Petri's run status word in the shared vocabulary.
fn petri_run_status(status: &str) -> String {
    match status {
        "success" => "succeeded".to_owned(),
        other => other.to_owned(),
    }
}

/// Petri's node status word in the shared vocabulary.
fn petri_stage_outcome(status: &str) -> String {
    match status {
        "success" => "succeeded".to_owned(),
        "partial_success" => "partially_succeeded".to_owned(),
        "failure" => "failed".to_owned(),
        other => other.to_owned(),
    }
}

fn node_name(instance: &str) -> String {
    instance.split('#').next().unwrap_or(instance).to_owned()
}

fn split_context(
    raw: &Map<String, Value>,
    rules: &Rules,
) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut context = BTreeMap::new();
    let mut bookkeeping = BTreeMap::new();
    for (key, value) in raw {
        if rules.is_bookkeeping(key) {
            bookkeeping.insert(key.clone(), value.clone());
        } else {
            context.insert(key.clone(), value.clone());
        }
    }
    (context, bookkeeping)
}

fn read_artifacts(workspace: &Path, rules: &Rules) -> BTreeMap<String, Option<String>> {
    rules
        .artifacts
        .iter()
        .map(|relative| {
            (
                relative.clone(),
                fs::read_to_string(workspace.join(relative)).ok(),
            )
        })
        .collect()
}

/// The platform call a request is, if any: the pinned Fabro asks the
/// provider's small default model for a run title before the first stage.
/// The prompt's opening sentence names it; nothing else is classified.
pub(crate) fn platform_request(log_record: &Value) -> Option<&'static str> {
    let text = log_record["input_text"].as_str().unwrap_or_default();
    text.starts_with("Generate a concise, human-readable title for this Fabro workflow run")
        .then_some("fabro.run_title")
}

/// The raw request bodies one engine's workflow sent to its twins, in
/// arrival order across twins, with the platform's own requests left out:
/// what an engine-specific request matcher inspects.
pub(crate) fn request_bodies(twins: &[&Twin], credential: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for twin in twins {
        let log = twin.request_log();
        for (index, body) in twin.requests_for(credential).into_iter().enumerate() {
            let record = log.get(index).cloned().unwrap_or(Value::Null);
            if platform_request(&record).is_none() {
                out.push(body);
            }
        }
    }
    out
}

/// Requests one engine made to its twins, in arrival order across twins:
/// the workflow's own, and the platform's apart.
pub(crate) fn requests_of(twins: &[&Twin], credential: &str) -> (Vec<Request>, Vec<Request>) {
    let mut own = Vec::new();
    let mut platform = Vec::new();
    for twin in twins {
        let log = twin.request_log();
        let bodies = twin.requests_for(credential);
        // The twin's log covers every namespace; align by arrival order of
        // this credential's bodies within the log.
        for (log_index, body) in bodies.iter().enumerate() {
            let record = log.get(log_index).cloned().unwrap_or(Value::Null);
            let request = Request {
                provider: twin.provider.id().to_owned(),
                model:    body["model"].as_str().unwrap_or_default().to_owned(),
                effort:   super::twins::requested_effort(twin.provider, body).map(str::to_owned),
                scenario: record["scenario_id"].as_str().map(str::to_owned),
            };
            if platform_request(&record).is_some() {
                platform.push(request);
            } else {
                own.push(request);
            }
        }
    }
    (own, platform)
}

/// Project a finished Petri run through `petri inspect --json`, the
/// workspace, the interview receipt and the twins' logs.
pub(crate) fn project_petri(
    finished: &Finished,
    workspace: &Path,
    twins: &[&Twin],
    credential: &str,
    rules: &Rules,
) -> Projection {
    let document = finished.inspect();
    let status = document["status"]
        .as_str()
        .map_or_else(|| "incomplete".to_owned(), petri_run_status);
    let invocations = document["invocations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let executions = document["executions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let root_id = document["root"]["invocation"].as_u64().unwrap_or(0);
    let histories = |invocation: u64| -> Vec<Value> {
        let mut records = Vec::new();
        for execution in &executions {
            if execution["invocation"].as_u64() != Some(invocation) {
                continue;
            }
            if let Some(history) = execution["engine"]["history"].as_array() {
                records.extend(history.iter().cloned());
            }
        }
        records
    };
    let stages_of = |records: &[Value]| -> Vec<Stage> {
        records
            .iter()
            .map(|record| Stage {
                node:    node_name(record["node"].as_str().unwrap_or_default()),
                outcome: petri_stage_outcome(record["status"].as_str().unwrap_or_default()),
            })
            .collect()
    };
    let root_history = histories(root_id);
    let path = stages_of(&root_history);

    // Forks: child invocations whose slot is `branch:<fork>:<index>:<node>`,
    // grouped by fork in declaration order; each fork's envelopes come from
    // the join's `parallel.results` update, matched by order of joins.
    let mut fork_order: Vec<String> = Vec::new();
    let mut branch_stages: BTreeMap<String, Vec<(u64, String, Vec<Stage>)>> = BTreeMap::new();
    for invocation in &invocations {
        let Some(slot) = invocation["parent"]["slot"].as_str() else {
            continue;
        };
        if invocation["parent"]["invocation"].as_u64() != Some(root_id) {
            continue;
        }
        let mut parts = slot.splitn(4, ':');
        if parts.next() != Some("branch") {
            continue;
        }
        let (Some(fork), Some(index), Some(node)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let index: u64 = index.parse().unwrap_or(0);
        let id = invocation["invocation"].as_u64().unwrap_or(0);
        if !fork_order.iter().any(|f| f == fork) {
            fork_order.push(fork.to_owned());
        }
        branch_stages.entry(fork.to_owned()).or_default().push((
            index,
            node.to_owned(),
            stages_of(&histories(id)),
        ));
    }
    // A joined list above Petri's fan-out threshold is published as a
    // reference; the comparison is of the logical list.
    let joins: Vec<Value> = root_history
        .iter()
        .filter_map(|record| record["context_updates"].get("parallel.results").cloned())
        .map(|value| inspect::resolve_reference(value, &finished.run_dir))
        .collect();
    let mut forks = Vec::new();
    for (position, fork) in fork_order.iter().enumerate() {
        let mut stages = branch_stages.remove(fork).unwrap_or_default();
        stages.sort_by_key(|(index, _, _)| *index);
        let results = joins
            .get(position)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let branches = stages
            .into_iter()
            .map(|(index, node, stages)| {
                let envelope = results
                    .iter()
                    .find(|r| r["index"].as_u64() == Some(index))
                    .cloned()
                    .unwrap_or(Value::Null);
                Branch {
                    id: envelope["id"].as_str().unwrap_or(&node).to_owned(),
                    index: envelope["index"].as_u64(),
                    item_label: envelope["item_label"].as_str().map(str::to_owned),
                    status: envelope["status"].as_str().unwrap_or("missing").to_owned(),
                    context_updates: envelope
                        .get("context_updates")
                        .cloned()
                        .unwrap_or(Value::Null),
                    stages,
                }
            })
            .collect();
        forks.push(Fork {
            node: fork.clone(),
            branches,
        });
    }

    let raw_context = inspect::root_context(&document)
        .as_object()
        .cloned()
        .unwrap_or_default();
    let (context, bookkeeping) = split_context(&raw_context, rules);
    let interviews = interviews_of(&document["interviews"]);
    let (requests, platform_requests) = requests_of(twins, credential);
    let mut counts = BTreeMap::new();
    counts.insert("provider_requests".to_owned(), requests.len() as u64);
    counts.insert(
        "platform_requests".to_owned(),
        platform_requests.len() as u64,
    );
    counts.insert("interviews_asked".to_owned(), interviews.len() as u64);
    counts.insert(
        "interviews_delivered".to_owned(),
        interviews
            .iter()
            .filter(|i| i.delivery == "delivered")
            .count() as u64,
    );
    let mut identities = BTreeMap::new();
    identities.insert(
        "<WORKSPACE>".to_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    let mut projection = Projection {
        engine: "petri".to_owned(),
        status,
        path,
        forks,
        context,
        bookkeeping,
        artifacts: read_artifacts(workspace, rules),
        interviews,
        requests,
        platform_requests,
        counts,
        identities,
    };
    normalize(&mut projection);
    projection
}

/// A branch as its events record it: index, node, outcome.
type BranchRecord = (u64, String, String);

/// Project a Fabro run through `fabro events`, the dump, the run's working
/// directory, the adapter's receipt and the twins' logs.
pub(crate) fn project_fabro(
    run: &FabroRun,
    twins: &[&Twin],
    credential: &str,
    rules: &Rules,
) -> Projection {
    let status = run.status.clone();
    let mut path = Vec::new();
    let mut context_values: Map<String, Value> = Map::new();
    let mut fork_results: Vec<(String, Vec<Value>)> = Vec::new();
    let mut branch_records: BTreeMap<String, BTreeMap<String, BranchRecord>> = BTreeMap::new();
    for event in &run.events {
        let kind = event["event"].as_str().unwrap_or_default();
        let node = event["node_id"].as_str().unwrap_or_default().to_owned();
        let props = event["properties"].clone();
        let group = event["parallel_group_id"].as_str().map(str::to_owned);
        let branch = event["parallel_branch_id"].as_str().map(str::to_owned);
        match kind {
            "stage.completed" if branch.is_none() && !node.is_empty() => {
                path.push(Stage {
                    node:    node.clone(),
                    outcome: props["status"].as_str().unwrap_or("succeeded").to_owned(),
                });
                if let Some(values) = props["context_values"].as_object() {
                    context_values.clone_from(values);
                }
                if let Some(updates) = props["context_updates"].as_object() {
                    for (key, value) in updates {
                        context_values.insert(key.clone(), value.clone());
                    }
                }
            }
            "stage.failed"
                if branch.is_none()
                    && !node.is_empty()
                    && !props["will_retry"].as_bool().unwrap_or(false) =>
            {
                path.push(Stage {
                    node:    node.clone(),
                    outcome: "failed".to_owned(),
                });
            }
            "checkpoint.completed" => {
                if let Some(values) = props["context_values"].as_object() {
                    context_values.clone_from(values);
                }
            }
            "parallel.completed" => {
                let results = props["results"].as_array().cloned().unwrap_or_default();
                fork_results.push((node.clone(), results));
            }
            "parallel.branch.started" => {
                if let (Some(group), Some(branch)) = (group.clone(), branch.clone()) {
                    branch_records.entry(group).or_default().insert(
                        branch,
                        (
                            props["index"].as_u64().unwrap_or(0),
                            node.clone(),
                            "started".to_owned(),
                        ),
                    );
                }
            }
            "parallel.branch.completed" => {
                if let (Some(group), Some(branch)) = (group.clone(), branch.clone()) {
                    let entry = branch_records.entry(group).or_default();
                    let record = entry.entry(branch).or_insert((
                        props["index"].as_u64().unwrap_or(0),
                        node.clone(),
                        String::new(),
                    ));
                    props["status"]
                        .as_str()
                        .unwrap_or("succeeded")
                        .clone_into(&mut record.2);
                }
            }
            _ => {}
        }
    }
    // The dump carries the envelopes with inline values where the event log
    // offloaded `command.output` to a blob reference. Prefer it, matched by
    // the fork node name in the stage directory (`<rank>-<node>@<visit>`).
    let dumped = run.dumped_parallel_results();
    let mut forks = Vec::new();
    let mut groups: Vec<(String, BTreeMap<String, BranchRecord>)> =
        branch_records.into_iter().collect();
    // Groups are `<fork>@<visit>`; keep event order by the fork's first appearance.
    groups.sort_by_key(|(group, _)| {
        run.events
            .iter()
            .position(|e| e["parallel_group_id"].as_str() == Some(group))
            .unwrap_or(usize::MAX)
    });
    for (position, (group, records)) in groups.iter().enumerate() {
        let fork = group.split('@').next().unwrap_or(group).to_owned();
        let from_events = fork_results
            .iter()
            .filter(|(node, _)| *node == fork)
            .nth(
                groups[..position]
                    .iter()
                    .filter(|(g, _)| g.split('@').next() == Some(&fork))
                    .count(),
            )
            .map(|(_, results)| results.clone())
            .unwrap_or_default();
        let from_dump = dumped
            .iter()
            .find(|(dir, _)| {
                dir.split_once('-')
                    .is_some_and(|(_, rest)| rest == group.as_str())
            })
            .and_then(|(_, value)| value.as_array().cloned());
        let results = from_dump.unwrap_or(from_events);
        let mut ordered: Vec<BranchRecord> = records.values().cloned().collect();
        ordered.sort_by_key(|(index, _, _)| *index);
        let branches = ordered
            .into_iter()
            .map(|(index, node, outcome)| {
                let envelope = results
                    .iter()
                    .find(|r| r["index"].as_u64() == Some(index))
                    .cloned()
                    .unwrap_or(Value::Null);
                Branch {
                    id:              envelope["id"].as_str().unwrap_or(&node).to_owned(),
                    index:           envelope["index"].as_u64(),
                    item_label:      envelope["item_label"].as_str().map(str::to_owned),
                    status:          envelope["status"].as_str().unwrap_or("missing").to_owned(),
                    context_updates: envelope
                        .get("context_updates")
                        .cloned()
                        .unwrap_or(Value::Null),
                    stages:          vec![Stage {
                        node,
                        outcome: outcome.clone(),
                    }],
                }
            })
            .collect();
        forks.push(Fork {
            node: fork,
            branches,
        });
    }
    let (context, bookkeeping) = split_context(&context_values, rules);
    let interviews = interviews_of(&run.receipt);
    let (requests, platform_requests) = requests_of(twins, credential);
    let mut counts = BTreeMap::new();
    counts.insert("provider_requests".to_owned(), requests.len() as u64);
    counts.insert(
        "platform_requests".to_owned(),
        platform_requests.len() as u64,
    );
    counts.insert("interviews_asked".to_owned(), interviews.len() as u64);
    counts.insert(
        "interviews_delivered".to_owned(),
        interviews
            .iter()
            .filter(|i| i.delivery == "delivered")
            .count() as u64,
    );
    let mut identities = BTreeMap::new();
    if let Some(id) = &run.run_id {
        identities.insert("<RUN_ID>".to_owned(), id.clone());
    }
    identities.insert(
        "<WORKSPACE>".to_owned(),
        run.workspace.to_string_lossy().into_owned(),
    );
    let mut projection = Projection {
        engine: "fabro".to_owned(),
        status,
        path,
        forks,
        context,
        bookkeeping,
        artifacts: read_artifacts(&run.workspace, rules),
        interviews,
        requests,
        platform_requests,
        counts,
        identities,
    };
    normalize(&mut projection);
    projection
}

fn interviews_of(receipt: &Value) -> Vec<Interview> {
    receipt["questions"]
        .as_array()
        .map(|questions| {
            questions
                .iter()
                .map(|q| Interview {
                    node:     q["node"].as_str().unwrap_or_default().to_owned(),
                    kind:     q["kind"].as_str().unwrap_or_default().to_owned(),
                    text:     q["text"].as_str().unwrap_or_default().to_owned(),
                    options:  q["options"]
                        .as_array()
                        .map(|o| {
                            o.iter()
                                .map(|v| v.as_str().unwrap_or_default().to_owned())
                                .collect()
                        })
                        .unwrap_or_default(),
                    reply:    q.get("reply").cloned().unwrap_or(Value::Null),
                    delivery: q["delivery"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Replace every raw identity with its placeholder, everywhere a string
/// appears in the projection, and record blob references as identities.
fn normalize(projection: &mut Projection) {
    let mut replacements: Vec<(String, String)> = projection
        .identities
        .iter()
        .map(|(placeholder, raw)| (raw.clone(), placeholder.clone()))
        .collect();
    // Longest raw values first so a path prefix never shadows a longer one.
    replacements.sort_by_key(|(raw, _)| Reverse(raw.len()));
    let mut blobs: BTreeMap<String, String> = BTreeMap::new();
    // The identity map itself keeps the raw values; only the observations
    // are rewritten.
    let identities = mem::take(&mut projection.identities);
    let mut value = serde_json::to_value(&*projection).expect("a projection serializes");
    rewrite(&mut value, &replacements, &mut blobs);
    let mut rewritten: Projection =
        serde_json::from_value(value).expect("a rewritten projection deserializes");
    rewritten.identities = identities;
    for (placeholder, raw) in blobs {
        rewritten.identities.insert(placeholder, raw);
    }
    *projection = rewritten;
}

fn rewrite(
    value: &mut Value,
    replacements: &[(String, String)],
    blobs: &mut BTreeMap<String, String>,
) {
    match value {
        Value::String(text) => {
            for (raw, placeholder) in replacements {
                if !raw.is_empty() && text.contains(raw.as_str()) {
                    *text = text.replace(raw.as_str(), placeholder);
                }
            }
            if let Some(rest) = text.strip_prefix("blob://sha256/") {
                let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
                if hex.len() == 64 {
                    let n = blobs.len() + 1;
                    let placeholder = format!("<BLOB_{n}>");
                    blobs.insert(placeholder.clone(), text.clone());
                    *text = text.replace(&format!("blob://sha256/{hex}"), &placeholder);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite(item, replacements, blobs);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                rewrite(item, replacements, blobs);
            }
        }
        _ => {}
    }
}

/// One named disagreement between the two projections.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Difference {
    pub(crate) kind:     String,
    pub(crate) field:    String,
    pub(crate) petri:    Value,
    pub(crate) fabro:    Value,
    /// The decision id that accepts this difference, if any.
    pub(crate) decision: Option<String>,
}

fn diff(kind: &str, field: String, petri: impl Serialize, fabro: impl Serialize) -> Difference {
    Difference {
        kind: kind.to_owned(),
        field,
        petri: serde_json::to_value(petri).expect("serializes"),
        fabro: serde_json::to_value(fabro).expect("serializes"),
        decision: None,
    }
}

/// Compare two projections and return every difference, each marked with
/// the decision that accepts it when one applies to `scenario`.
pub(crate) fn compare(
    petri: &Projection,
    fabro: &Projection,
    scenario: &str,
    decisions: &Decisions,
) -> Vec<Difference> {
    let mut out = Vec::new();
    if petri.status != fabro.status {
        out.push(diff(
            "status",
            "status".into(),
            &petri.status,
            &fabro.status,
        ));
    }

    // The path: Petri records a `skipped` final outcome for a skipped stage
    // where Fabro records nothing, and lists each parallel branch's delegate
    // between the fork and the join where Fabro records branch events. Each
    // is its own named difference. The remaining sequences must match
    // exactly, in order.
    let branch_nodes: BTreeSet<&str> = petri
        .forks
        .iter()
        .flat_map(|fork| fork.branches.iter())
        .flat_map(|branch| {
            iter::once(branch.id.as_str())
                .chain(branch.stages.iter().map(|stage| stage.node.as_str()))
        })
        .collect();
    let fabro_nodes: BTreeSet<&str> = fabro.path.iter().map(|stage| stage.node.as_str()).collect();
    let mut petri_path = Vec::new();
    for stage in &petri.path {
        if stage.outcome == "skipped" {
            out.push(diff(
                "path.skipped_stage",
                format!("path[{}]", stage.node),
                stage,
                Value::Null,
            ));
        } else if branch_nodes.contains(stage.node.as_str())
            && !fabro_nodes.contains(stage.node.as_str())
        {
            out.push(diff(
                "path.branch_stage",
                format!("path[{}]", stage.node),
                stage,
                Value::Null,
            ));
        } else {
            petri_path.push(stage.clone());
        }
    }
    if petri_path != fabro.path {
        out.push(diff("path.order", "path".into(), &petri_path, &fabro.path));
    }

    if petri.forks.len() != fabro.forks.len() {
        out.push(diff(
            "fork.count",
            "forks".into(),
            petri.forks.iter().map(|f| &f.node).collect::<Vec<_>>(),
            fabro.forks.iter().map(|f| &f.node).collect::<Vec<_>>(),
        ));
    }
    for (p, f) in petri.forks.iter().zip(&fabro.forks) {
        let field = format!("forks[{}]", p.node);
        if p.node != f.node {
            out.push(diff("fork.node", field.clone(), &p.node, &f.node));
        }
        if p.branches.len() != f.branches.len() {
            out.push(diff(
                "branch.count",
                field.clone(),
                p.branches.len(),
                f.branches.len(),
            ));
        }
        for (pb, fb) in p.branches.iter().zip(&f.branches) {
            let field = format!("{field}.branches[{}]", pb.id);
            if pb.id != fb.id {
                out.push(diff("branch.id", field.clone(), &pb.id, &fb.id));
            }
            if pb.index != fb.index {
                out.push(diff("branch.index", field.clone(), pb.index, fb.index));
            }
            if pb.item_label != fb.item_label {
                out.push(diff(
                    "branch.item_label",
                    field.clone(),
                    &pb.item_label,
                    &fb.item_label,
                ));
            }
            if pb.status != fb.status {
                out.push(diff("branch.status", field.clone(), &pb.status, &fb.status));
            }
            compare_values(
                &mut out,
                "branch.context_updates",
                &format!("{field}.context_updates"),
                &pb.context_updates,
                &fb.context_updates,
            );
            if pb.stages != fb.stages {
                out.push(diff(
                    "branch.stages",
                    format!("{field}.stages"),
                    &pb.stages,
                    &fb.stages,
                ));
            }
        }
    }

    for key in petri
        .context
        .keys()
        .chain(fabro.context.keys())
        .collect::<BTreeSet<_>>()
    {
        match (petri.context.get(key), fabro.context.get(key)) {
            (Some(p), Some(f)) => {
                compare_values(&mut out, "context.value", &format!("context.{key}"), p, f);
            }
            (Some(p), None) => out.push(diff(
                "context.missing_in_fabro",
                format!("context.{key}"),
                p,
                Value::Null,
            )),
            (None, Some(f)) => out.push(diff(
                "context.missing_in_petri",
                format!("context.{key}"),
                Value::Null,
                f,
            )),
            (None, None) => {}
        }
    }

    for (path, p) in &petri.artifacts {
        let f = fabro.artifacts.get(path).cloned().flatten();
        if *p != f {
            out.push(diff("artifact", format!("artifacts.{path}"), p, f));
        }
    }

    if petri.interviews.len() != fabro.interviews.len() {
        out.push(diff(
            "interview.count",
            "interviews".into(),
            petri.interviews.len(),
            fabro.interviews.len(),
        ));
    }
    for (index, (p, f)) in petri.interviews.iter().zip(&fabro.interviews).enumerate() {
        let field = format!("interviews[{index}]");
        if p.node != f.node {
            out.push(diff("interview.node", field.clone(), &p.node, &f.node));
        }
        if p.kind != f.kind {
            out.push(diff("interview.kind", field.clone(), &p.kind, &f.kind));
        }
        if p.text != f.text {
            out.push(diff("interview.text", field.clone(), &p.text, &f.text));
        }
        if p.options != f.options {
            out.push(diff(
                "interview.options",
                field.clone(),
                &p.options,
                &f.options,
            ));
        }
        if p.reply != f.reply {
            out.push(diff("interview.reply", field.clone(), &p.reply, &f.reply));
        }
        if p.delivery != f.delivery {
            out.push(diff(
                "interview.delivery",
                field.clone(),
                &p.delivery,
                &f.delivery,
            ));
        }
    }

    if petri.requests.len() != fabro.requests.len() {
        out.push(diff(
            "request.count",
            "requests".into(),
            &petri.requests,
            &fabro.requests,
        ));
    }
    for (index, (p, f)) in petri.requests.iter().zip(&fabro.requests).enumerate() {
        if p != f {
            out.push(diff("request", format!("requests[{index}]"), p, f));
        }
    }

    for (name, p) in &petri.counts {
        let f = fabro.counts.get(name).copied().unwrap_or(0);
        if *p != f {
            out.push(diff("count", format!("counts.{name}"), p, f));
        }
    }

    for difference in &mut out {
        difference.decision = decisions.accepting(difference, scenario);
    }
    out
}

/// Compare two JSON values leaf by leaf, so a difference names the exact
/// key. Objects recurse; everything else compares whole.
fn compare_values(
    out: &mut Vec<Difference>,
    kind: &str,
    field: &str,
    petri: &Value,
    fabro: &Value,
) {
    match (petri, fabro) {
        (Value::Object(p), Value::Object(f)) => {
            for key in p.keys().chain(f.keys()).collect::<BTreeSet<_>>() {
                let field = format!("{field}.{key}");
                match (p.get(key), f.get(key)) {
                    (Some(pv), Some(fv)) => compare_values(out, kind, &field, pv, fv),
                    (Some(pv), None) => out.push(diff(
                        &format!("{kind}.missing_in_fabro"),
                        field,
                        pv,
                        Value::Null,
                    )),
                    (None, Some(fv)) => out.push(diff(
                        &format!("{kind}.missing_in_petri"),
                        field,
                        Value::Null,
                        fv,
                    )),
                    (None, None) => {}
                }
            }
        }
        (Value::String(p), Value::String(f)) if p != f => {
            // A value equal to Fabro's plus one final newline is its own
            // named difference (once a command output departure, now
            // retired and accepted by no record, so a recurrence is named
            // as itself); any other string difference is the general kind.
            let kind = if p.len() == f.len() + 1 && p.strip_suffix('\n') == Some(f.as_str()) {
                "value.trailing_newline".to_owned()
            } else {
                kind.to_owned()
            };
            out.push(diff(&kind, field.to_owned(), petri, fabro));
        }
        _ if petri != fabro => out.push(diff(kind, field.to_owned(), petri, fabro)),
        _ => {}
    }
}

/// One committed decision record.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Decision {
    pub(crate) id:                  String,
    pub(crate) title:               String,
    #[serde(default = "all")]
    pub(crate) scenarios:           Vec<String>,
    #[serde(default = "all")]
    pub(crate) bundles:             Vec<String>,
    pub(crate) fabro:               String,
    pub(crate) petri:               String,
    pub(crate) user_visible_effect: String,
    pub(crate) reason:              String,
    pub(crate) acceptance:          String,
    #[serde(default)]
    pub(crate) migration:           Option<Migration>,
    #[serde(default)]
    pub(crate) accepts:             Vec<Accepts>,
}

fn all() -> Vec<String> {
    vec!["*".to_owned()]
}

/// A migration names the bundle versions it moves between.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Migration {
    pub(crate) old_bundle: String,
    pub(crate) new_bundle: String,
}

/// One difference shape a decision accepts: the exact kind, and optionally
/// a field pattern (`*` matches any run of characters).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Accepts {
    pub(crate) kind:  String,
    #[serde(default)]
    pub(crate) field: Option<String>,
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => {
            let Some(rest) = text.strip_prefix(head) else {
                return false;
            };
            (0..=rest.len())
                .any(|skip| rest.is_char_boundary(skip) && glob_matches(tail, &rest[skip..]))
        }
    }
}

/// The decision index: every committed decision record.
#[derive(Clone, Debug, Default)]
pub(crate) struct Decisions {
    pub(crate) records: Vec<Decision>,
}

impl Decisions {
    /// The directory the records live in.
    pub(crate) fn dir() -> PathBuf {
        repo_root().join("crates/fabro/acceptance/decisions")
    }

    /// Load every `*.toml` record. A record that does not parse fails the
    /// test: a malformed decision must not silently accept nothing or
    /// everything.
    pub(crate) fn load() -> Self {
        let dir = Self::dir();
        let mut records = Vec::new();
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        entries.sort();
        for path in entries {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let record: Decision = toml::from_str(&text).unwrap_or_else(|error| {
                panic!("{} is not a decision record: {error}", path.display())
            });
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            assert_eq!(
                record.id,
                stem,
                "{}: the record id must equal the file name",
                path.display()
            );
            records.push(record);
        }
        Self { records }
    }

    /// The id of the first decision that accepts `difference` for
    /// `scenario`, if any.
    pub(crate) fn accepting(&self, difference: &Difference, scenario: &str) -> Option<String> {
        self.records
            .iter()
            .find(|decision| {
                decision
                    .scenarios
                    .iter()
                    .any(|pattern| glob_matches(pattern, scenario))
                    && decision.accepts.iter().any(|accepts| {
                        accepts.kind == difference.kind
                            && accepts
                                .field
                                .as_deref()
                                .is_none_or(|pattern| glob_matches(pattern, &difference.field))
                    })
            })
            .map(|decision| decision.id.clone())
    }
}

/// The differences no decision accepts.
pub(crate) fn unresolved(differences: &[Difference]) -> Vec<&Difference> {
    differences
        .iter()
        .filter(|d| d.decision.is_none())
        .collect()
}

/// Render differences for a failure message.
pub(crate) fn render(differences: &[&Difference]) -> String {
    differences
        .iter()
        .map(|d| {
            format!(
                "- {} at {}\n    petri: {}\n    fabro: {}",
                d.kind,
                d.field,
                serde_json::to_string(&d.petri).unwrap_or_default(),
                serde_json::to_string(&d.fabro).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The provider a request went to, for engine-specific request matchers.
pub(crate) fn provider_of(request: &Request) -> Option<Provider> {
    match request.provider.as_str() {
        "openai" => Some(Provider::OpenAi),
        "anthropic" => Some(Provider::Anthropic),
        "openrouter" => Some(Provider::OpenRouter),
        _ => None,
    }
}

/// A projection as a JSON value, for evidence records.
pub(crate) fn to_value(projection: &Projection) -> Value {
    serde_json::to_value(projection).expect("a projection serializes")
}

/// A projection from a JSON value (a committed reference).
pub(crate) fn from_value(value: &Value) -> Result<Projection, String> {
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

/// Pointer-by-pointer differences between two JSON documents, for a
/// reviewable baseline diff.
pub(crate) fn json_differences(left: &Value, right: &Value) -> Vec<String> {
    let mut out = Vec::new();
    json_diff_into(&mut out, "", left, right);
    out
}

fn json_diff_into(out: &mut Vec<String>, pointer: &str, left: &Value, right: &Value) {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for key in l.keys().chain(r.keys()).collect::<BTreeSet<_>>() {
                let pointer = format!("{pointer}/{key}");
                match (l.get(key), r.get(key)) {
                    (Some(lv), Some(rv)) => json_diff_into(out, &pointer, lv, rv),
                    (Some(lv), None) => out.push(format!("{pointer}: only in the run: {lv}")),
                    (None, Some(rv)) => out.push(format!("{pointer}: only in the reference: {rv}")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(l), Value::Array(r)) => {
            if l.len() != r.len() {
                out.push(format!(
                    "{pointer}: {} items in the run, {} in the reference",
                    l.len(),
                    r.len()
                ));
            }
            for (index, (lv, rv)) in l.iter().zip(r).enumerate() {
                json_diff_into(out, &format!("{pointer}/{index}"), lv, rv);
            }
        }
        _ if left != right => out.push(format!("{pointer}: run {left} vs reference {right}")),
        _ => {}
    }
}

pub(crate) fn empty_rules() -> Rules {
    Rules::default()
}

pub(crate) fn rules(bookkeeping: &[&str], artifacts: &[&str]) -> Rules {
    Rules {
        bookkeeping: bookkeeping.iter().map(|s| (*s).to_owned()).collect(),
        artifacts:   artifacts.iter().map(|s| (*s).to_owned()).collect(),
    }
}

pub(crate) fn json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).expect("serializes")
}

#[cfg(test)]
mod tests {
    use super::glob_matches;

    #[test]
    fn globs_match_prefixes_and_infixes() {
        assert!(glob_matches("context.*", "context.command.output"));
        assert!(glob_matches("*.command.output", "branch.x.command.output"));
        assert!(glob_matches(
            "forks[*].branches[*].context_updates.command.output",
            "forks[find].branches[finder_a].context_updates.command.output"
        ));
        assert!(!glob_matches("context.*", "artifacts.report"));
        assert!(glob_matches("status", "status"));
        assert!(!glob_matches("status", "status2"));
    }
}
