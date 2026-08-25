//! A tiny host for the pure core: it turns commands into scripted step results and
//! feeds them back, so tests read as "run this graph and see what happened".

#![allow(dead_code)]

use std::collections::BTreeMap;

use engine::{Command, EngineState, Event, apply};
use ir::{
    FiringId, Generation, Graph, NodeId, Outcome, RunStatus, StepKind, StepKindId, StepRegistry,
    Token, Value,
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
        StepKindId::new(0)
    }
    fn name(&self) -> &str {
        "noop"
    }
}

pub fn registry() -> StepRegistry {
    let mut registry = StepRegistry::new();
    registry.register(Box::new(Noop));
    registry
}

pub const NOOP: StepKindId = StepKindId::new(0);

type Responder = Box<dyn FnMut(&StartInfo) -> Outcome>;

pub struct Harness {
    pub state: EngineState,
    responder: Responder,
    /// Every command the core produced, in order.
    pub commands: Vec<Command>,
    /// Names of the nodes the host was told to start, in order.
    pub started: Vec<String>,
    /// The most steps that were running at the same time.
    pub max_concurrent: usize,
    pub status: Option<RunStatus>,
}

impl Harness {
    pub fn new(graph: Graph) -> Self {
        Self {
            state: EngineState::new(graph),
            responder: Box::new(|_| Outcome::success(Value::Null)),
            commands: Vec::new(),
            started: Vec::new(),
            max_concurrent: 0,
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
                .filter(|c| matches!(c, Command::StartStep { .. }))
                .cloned()
                .collect();
            self.commands
                .retain(|c| !matches!(c, Command::StartStep { .. }));
            if pending.is_empty() {
                break;
            }
            self.max_concurrent = self.max_concurrent.max(pending.len());
            for command in pending {
                let Command::StartStep {
                    firing,
                    node,
                    generation,
                    inputs,
                    config,
                    ..
                } = command
                else {
                    continue;
                };
                let name = self
                    .state
                    .graph
                    .node(node)
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                let (base, index) = split_clone_name(&name);
                let info = StartInfo {
                    firing,
                    node,
                    name: name.clone(),
                    base,
                    index,
                    generation,
                    config,
                    inputs,
                };
                self.started.push(name);
                let outcome = (self.responder)(&info);
                self.feed(Event::StepStarted { firing });
                self.feed(Event::StepFinished { firing, outcome });
                if let Some(status) = self.status {
                    return status;
                }
            }
        }
        self.status.unwrap_or_else(|| self.state.folded_status())
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
                Command::StartStep { firing, node, .. } => Some((
                    *firing,
                    self.state
                        .graph
                        .node(*node)
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

    /// Report a step's result to the core.
    pub fn finish(&mut self, firing: FiringId, outcome: Outcome) {
        self.feed(Event::StepStarted { firing });
        self.feed(Event::StepFinished { firing, outcome });
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

fn split_clone_name(name: &str) -> (String, Option<u32>) {
    match name.rsplit_once('#') {
        Some((base, index)) => match index.parse::<u32>() {
            Ok(index) => (base.to_string(), Some(index)),
            Err(_) => (name.to_string(), None),
        },
        None => (name.to_string(), None),
    }
}
