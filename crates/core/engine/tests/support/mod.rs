//! A tiny host for the pure core: it turns commands into scripted step results and
//! feeds them back, so tests read as "run this graph and see what happened".

#![allow(dead_code)]

use std::collections::BTreeMap;

use engine::{Command, EngineState, Event, apply};
use ir::{
    Attempt, FiringId, Generation, Graph, NodeId, Outcome, RunStatus, StepKind, StepKindId,
    StepKinds, Token, Value,
};

/// What the host is being asked to run.
#[derive(Clone, Debug)]
pub struct StartInfo {
    pub firing: FiringId,
    pub node: NodeId,
    /// Node name, including the `#index` suffix on expansion clones.
    pub name: String,
    /// Node name with any `#index` suffix removed.
    pub base: String,
    /// Clone index, when this node came out of an expansion.
    pub index: Option<u32>,
    pub generation: Generation,
    /// Which try this is, 1-based.
    pub attempt: Attempt,
    pub config: Value,
    pub inputs: Vec<Token>,
}

impl StartInfo {
    /// The payload of the first input token.
    pub fn input(&self) -> Value {
        self.inputs
            .first()
            .map(|t| t.payload.clone())
            .unwrap_or(Value::Null)
    }
}

/// The single step kind the tests use. Nodes differ by config, not by kind.
pub struct Noop;

impl StepKind for Noop {
    fn id(&self) -> StepKindId {
        NOOP
    }
    fn name(&self) -> &str {
        "noop"
    }
}

/// The load-time lookup `validate_with` takes, over the one test kind.
pub struct Kinds(Vec<Box<dyn StepKind>>);

impl StepKinds for Kinds {
    fn get(&self, id: &StepKindId) -> Option<&dyn StepKind> {
        self.0.iter().find(|k| k.id() == *id).map(|k| k.as_ref())
    }
}

pub fn registry() -> Kinds {
    Kinds(vec![Box::new(Noop)])
}

pub const NOOP: StepKindId = StepKindId::new_static("noop");

type Responder = Box<dyn FnMut(&StartInfo) -> Outcome>;

pub struct Harness {
    pub state: EngineState,
    /// The graph the run started from, before any splice. Replay needs this one.
    pub original_graph: Graph,
    responder: Responder,
    /// Every command the core produced, in order.
    pub commands: Vec<Command>,
    /// Names of the nodes the host was told to start, in order.
    pub started: Vec<String>,
    /// The most steps that were running at the same time.
    pub max_concurrent: usize,
    /// Every `ScheduleRetry` the core issued, in order.
    pub scheduled_retries: Vec<(FiringId, Attempt, std::time::Duration)>,
    pub status: Option<RunStatus>,
}

impl Harness {
    pub fn new(graph: Graph) -> Self {
        Self {
            original_graph: graph.clone(),
            state: EngineState::new(graph),
            responder: Box::new(|_| Outcome::success(Value::Null)),
            commands: Vec::new(),
            started: Vec::new(),
            max_concurrent: 0,
            scheduled_retries: Vec::new(),
            status: None,
        }
    }

    /// Decide each step's result from the request.
    pub fn respond_with(mut self, f: impl FnMut(&StartInfo) -> Outcome + 'static) -> Self {
        self.responder = Box::new(f);
        self
    }

    /// Fixed results per node base name; anything unlisted succeeds with `null`.
    pub fn results(self, results: BTreeMap<&'static str, Outcome>) -> Self {
        let table: BTreeMap<String, Outcome> = results
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        self.respond_with(move |info| {
            table
                .get(&info.base)
                .cloned()
                .unwrap_or_else(|| Outcome::success(Value::Null))
        })
    }

    /// Start the run and pump until the core reports it finished.
    pub fn run(&mut self) -> RunStatus {
        self.feed(Event::RunStarted);
        // Steps are held until the whole batch is issued, so `max_concurrent`
        // reflects what the core allowed to run at once, not the order the host
        // happened to reply in.
        loop {
            let pending: Vec<Command> = self
                .commands
                .iter()
                .filter(|c| matches!(c, Command::StartStep(_)))
                .cloned()
                .collect();
            self.commands
                .retain(|c| !matches!(c, Command::StartStep { .. }));
            if pending.is_empty() {
                break;
            }
            self.max_concurrent = self.max_concurrent.max(pending.len());
            for command in pending {
                let Command::StartStep(resolved) = command else {
                    continue;
                };
                let name = self
                    .state
                    .graph
                    .node(resolved.node())
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                let (base, index) = split_clone_name(&name);
                let firing = resolved.id();
                let attempt = resolved.attempt();
                let info = StartInfo {
                    firing,
                    node: resolved.node(),
                    name: name.clone(),
                    base,
                    index,
                    generation: resolved.generation(),
                    attempt,
                    config: resolved.config().clone(),
                    inputs: resolved.inputs().to_vec(),
                };
                self.started.push(name);
                let outcome = (self.responder)(&info);
                self.feed(Event::StepStarted { firing, attempt });
                self.feed(Event::StepFinished {
                    firing,
                    attempt,
                    outcome,
                });
                // A retry keeps the firing live; walk the backoff without a clock.
                self.drain_retries();
                if let Some(status) = self.status {
                    return status;
                }
            }
        }
        self.status.unwrap_or_else(|| self.state.folded_status())
    }

    /// Answer every outstanding `ScheduleRetry` at once. The driver would sleep and
    /// add jitter; a test just feeds the event straight back.
    pub fn drain_retries(&mut self) {
        loop {
            let pending: Vec<(FiringId, Attempt, std::time::Duration)> = self
                .commands
                .iter()
                .filter_map(|c| match c {
                    Command::ScheduleRetry {
                        firing,
                        next_attempt,
                        base_delay,
                    } => Some((*firing, *next_attempt, *base_delay)),
                    _ => None,
                })
                .collect();
            if pending.is_empty() {
                return;
            }
            self.commands
                .retain(|c| !matches!(c, Command::ScheduleRetry { .. }));
            self.scheduled_retries.extend(pending.iter().copied());
            for (firing, next_attempt, _) in pending {
                self.feed(Event::RetryElapsed {
                    firing,
                    next_attempt,
                });
            }
        }
    }

    /// Push one event through the core, keeping the commands it produced.
    pub fn feed(&mut self, event: Event) {
        let state = std::mem::replace(&mut self.state, EngineState::new(Graph::new()));
        let (state, commands) = apply(state, event);
        self.state = state;
        for command in &commands {
            if let Command::FinishRun { status } = command {
                self.status = Some(*status);
            }
        }
        self.commands.extend(commands);
    }

    /// Cancel a scope mid-run, then keep pumping.
    pub fn cancel(&mut self, scope: ir::CancelScopeId) {
        self.feed(Event::CancelRequested { scope });
    }

    /// Pull the `StartStep` commands issued so far, as `(firing, node name)`.
    /// Used by tests that drive the core one event at a time.
    pub fn take_starts(&mut self) -> Vec<(FiringId, String)> {
        let starts: Vec<(FiringId, String)> = self
            .commands
            .iter()
            .filter_map(|c| match c {
                Command::StartStep(resolved) => Some((
                    resolved.id(),
                    self.state
                        .graph
                        .node(resolved.node())
                        .map(|n| n.name.to_string())
                        .unwrap_or_default(),
                )),
                _ => None,
            })
            .collect();
        self.commands
            .retain(|c| !matches!(c, Command::StartStep { .. }));
        for (_, name) in &starts {
            self.started.push(name.clone());
        }
        starts
    }

    /// Report a step's result to the core, for the attempt it is running.
    pub fn finish(&mut self, firing: FiringId, outcome: Outcome) {
        let attempt = self
            .state
            .firing(firing)
            .map(|f| f.attempt)
            .unwrap_or(Attempt::FIRST);
        self.feed(Event::StepStarted { firing, attempt });
        self.feed(Event::StepFinished {
            firing,
            attempt,
            outcome,
        });
    }

    /// Replay the log from a fresh state and check it comes back byte-identical.
    ///
    /// The determinism canary: if any core decision depended on a clock, on
    /// iteration order, or on anything outside the state, the logs diverge.
    pub fn verify_replay(&self) {
        if let Err(mismatch) = engine::verify_replay(self.original_graph.clone(), &self.state.log) {
            panic!("replay was not byte-identical: {mismatch}");
        }
    }

    pub fn output(&self, node: &str) -> Value {
        self.state.output(node).cloned().unwrap_or(Value::Null)
    }

    /// How many times a node base name was started.
    pub fn start_count(&self, base: &str) -> usize {
        self.started
            .iter()
            .filter(|n| split_clone_name(n).0 == base)
            .count()
    }

    pub fn statuses(&self) -> Vec<(String, String)> {
        self.state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag().to_string()))
            .collect()
    }

    pub fn status_of(&self, name: &str) -> Option<String> {
        self.state
            .history()
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.outcome.status.tag().to_string())
    }

    pub fn commands_of<T>(&self, f: impl Fn(&Command) -> Option<T>) -> Vec<T> {
        self.commands.iter().filter_map(f).collect()
    }
}

/// Stand-in for the process step kind's `soft_fail` handling.
///
/// Producing `PartialSuccess` is a step-kind decision, not a core one: there is no
/// node-level policy. This is the shape BuildKite's `soft_fail` and GHA's
/// `continue-on-error` both lower onto.
pub fn process_outcome(config: &Value, exit_code: i32) -> Outcome {
    if exit_code == 0 {
        return Outcome::success(serde_json::json!({ "exit_code": 0 }));
    }
    let failure = ir::FailureInfo::exit_status(exit_code);
    let soft = match config.get("soft_fail") {
        Some(Value::Bool(true)) => true,
        Some(Value::Array(codes)) => codes
            .iter()
            .any(|c| c.as_i64() == Some(i64::from(exit_code))),
        _ => false,
    };
    let output = serde_json::json!({ "exit_code": exit_code });
    if soft {
        // The real failure rides along in `underlying`, so the log never records a
        // clean success for something that failed.
        Outcome::partial(failure, output)
    } else {
        Outcome::new(ir::Status::Failure(failure), output)
    }
}

fn split_clone_name(name: &str) -> (String, Option<u32>) {
    match name.rsplit_once('#') {
        Some((base, index)) => match index.parse::<u32>() {
            Ok(index) => (base.to_string(), Some(index)),
            Err(_) => (name.to_string(), None),
        },
        None => (name.to_string(), None),
    }
}
