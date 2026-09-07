# The `petri inspect` document

`petri inspect --run-dir <directory> --json` reconstructs a run from the
durable files in its run directory and prints one JSON document. The library
form is `execution::inspect::inspect_run`. This file is the field contract for
`inspect_format_version` 1.

## Sources

The command reads only these files:

- `run.json`: the run's format version and root invocation id.
- `coordinator.jsonl`: graph registrations, invocation and execution
  declarations, exits, results, cancellation requests, and the run's finish.
- `graphs/<digest>.json`: every registered graph, byte-exact.
- `invocations/<invocation>/executions/<execution>/events.jsonl`: one engine
  log per execution.
- `interviews.json`: the interview receipt the standalone host writes when
  the run had an interviewer (`execution::InterviewReceipt`). Optional.

It decodes them under the store's own rules and replays each engine log
through `engine::replay`. The loaded log must be a byte-prefix of the log
replay regenerates. Run context is derived on every call and is never stored
separately.

The command does not start a step, acquire a sandbox, contact a provider,
take the run lease, or write under the run directory. It works on a run
another process still holds, on a machine with no provider reachable, and
after the source workflow file has changed or been deleted.

## Outcomes

- Exit 0 with a document whose `complete` is `true`: the run recorded its
  finish, every log decoded whole, and every execution replayed exactly.
- Exit 1 with a document whose `complete` is `false`: the run is interrupted
  or its logs are short. `incomplete` lists every reason. The document still
  reports what the logs support.
- Exit 2 with an error on stderr and no document: the files do not support a
  trustworthy reconstruction. Causes: a missing or unreadable `run.json` or
  `coordinator.jsonl`, an unsupported format version, a complete record that
  does not decode, a coordinator log that does not replay, a registered graph
  that is missing or does not match its digest, an engine log with an
  undecodable record or version, an engine log that diverges from replay, or
  an `interviews.json` that is not an interview receipt.

The torn-line rule is the store's: only a final line that ends without a
newline is dropped, and the file is left as found. A newline-terminated line
that does not decode is corruption and is an error.

## Secrets

Values appear as the driver appended them, after masking. A secret reference
stays `{"$secret": "<name>"}` and a masked value stays `***`. The document
never carries a resolved secret. Child secret bindings show as names only. A
sensitive human-gate answer therefore appears twice: as the `$secret`
reference in `deliveries`, and as `***` wherever the step echoed the value.

## Fields

Ids are plain integers: invocation and execution ids, firing ids, node ids,
edge ids, generations, and attempts. Status words are the engine's lowercase
tags. Run statuses are `success`, `failed`, `cancelled`. Node statuses are
`success`, `partial_success`, `failure`, `skipped`, `cancelled`, `timed_out`.

### Top level

| Field | Meaning |
|---|---|
| `inspect_format_version` | This document's version. `1`. |
| `coordinator_format_version` | The run directory's own format, from `run.json`. |
| `run_dir` | The directory as named on the command line. |
| `complete` | `true` only when `incomplete` is empty. |
| `status` | The recorded run status, or `null` until the run finished. |
| `incomplete` | Every reason `complete` is `false`, in the order found. |
| `root` | `invocation`, `final_execution` (the execution the root's result names, or `null`), `latest_execution` (the root's newest execution). |
| `middleware_chain` | The configured decision middleware, by key. |
| `graphs` | Every registered graph digest, sorted. |
| `invocations` | Every invocation, in id order. The root is first. |
| `executions` | Every execution, in id order, which is declaration order across the run. |
| `interviews` | The interview receipt (`version`, `questions`, `errors`, `script`) as the host wrote it, or `null` when the run had no interviewer. Sensitive answers appear only as `$secret` references. |

### Invocation

| Field | Meaning |
|---|---|
| `invocation` | The id. |
| `status` | `declared`, `running`, or `finished`. |
| `parent` | `null` on the root. Otherwise the call that declared it: `invocation`, `execution`, `firing`, `attempt`, `slot`. |
| `children` | Invocations whose call site lies in one of this invocation's executions. |
| `graph` | The digest of the graph it runs. |
| `context` | The context it was declared with. |
| `secrets` | `mode` (`none`, `inherit`, `explicit`) and `names` (the explicit binding names). |
| `sandbox` | `isolated` or `inherited`. |
| `cancel_requested` | Whether the coordinator recorded a cancel request. |
| `cancel_reason` | Why, when the requester said: `{"kind": "interrupt"}` (Ctrl-C), `{"kind": "control"}` (a run control), or `{"kind": "stall_timeout", "stall_timeout_ms", "idle_ms"}` (the watchdog). Absent for a plain cancel. Additive in format version 1. |
| `executions` | Every execution, in order. Each after the first followed a restart. |
| `final_execution` | The execution the result names, or `null`. |
| `result` | `null` until finished. Otherwise `status`, `failure`, `final_execution`, `output`, `context` (the final execution's `kv`, as recorded). |

### Execution

| Field | Meaning |
|---|---|
| `execution`, `invocation` | Ids. |
| `execution_index` | Position within the invocation, from 0. |
| `predecessor`, `successor` | Neighbouring executions in the invocation, or `null`. |
| `status` | `finished` (terminal exit recorded), `restarted` (restart exit recorded), or `incomplete`. |
| `entry_node` | The node a restart successor starts at, or `null` for the graph's own entries. |
| `start_context` | The context the execution started with. |
| `exit` | The exit the coordinator recorded, or `null`. `{"kind": "terminal", "status"}` or `{"kind": "restart", "edge", "target", "target_name", "source_firing"}`. |
| `log` | `path` (relative to the run directory), `records`, `torn`, `replay` (`verified`, `prefix`, or `missing`). |
| `engine` | The replayed state, or `null` when there is no log. |
| `children` | Invocations whose call site is a firing of this execution. |

`log.replay` is `verified` when replay regenerated exactly the log's records,
`prefix` when the log is a byte-prefix of the regenerated log (a crash landed
between an external append and the flush of its derived records), and
`missing` when there is no log or an empty file. `prefix` and `missing` make
the run incomplete.

### Engine

| Field | Meaning |
|---|---|
| `started`, `finished`, `cancelled` | Replayed state flags. `cancelled` means the root cancel scope was cancelled. |
| `exit` | The exit replay derived. Same shape as the execution's `exit`. A disagreement between the two is listed in `incomplete`. |
| `folded_status` | The run status folded from node outcomes under the graph's completion policy. |
| `errors` | Engine errors, rendered. |
| `context` | `kv` and `nodes`. |
| `history` | Every final firing record in completion order. |
| `attempts` | Every `StepFinished` record, final or not, in log order. |
| `routes` | Every applied route, in log order. |
| `deliveries` | Every control the host delivered into a firing, in log order. |
| `live` | Firings still live when the log ends. Empty for a finished execution. |

`context.nodes` is keyed by node instance name (`build`, `build#2`). Each
record has `status`, `success_like`, `failure`, `output`, `generation`, and
`attempts`. This is the engine's node-instance record: it reflects only the
final attempt of the latest generation to complete.

`history` entries have `firing`, `node`, `node_id`, `generation`, `attempt`,
`status`, `failure`, `output`, and `context_updates`. One entry per firing.

`attempts` entries have `seq` (the record's position in the log), `firing`,
`node`, `generation`, `attempt`, `status`, `failure`, and `final`. Retries
show up here and nowhere else: a non-final attempt has `final: false`, and
its `context_updates` never reached `kv`.

`routes` entries have `seq`, `firing`, `node`, `kind` (`edge`, `jump`,
`none`), `group`, `edge`, and `target` (the node the route led to). Together
with `history` they show routing across repeated visits to one node.

`deliveries` entries have `seq`, `firing`, `node`, `kind` (`deliver`,
`cancel`, `kill`, `other`), and `payload` (the delivered value, for `deliver`).
A sensitive answer was delivered as `{"$secret": "answer:<question>"}` and is
shown as that reference. The step's own echo of the value, in its output or
`context_updates`, was masked to `***` before it was appended.

`live` entries have `firing`, `node`, `generation`, `attempt`, `started`,
`awaiting_retry`, and `cancelling`.

## Versioning

`inspect_format_version` is bumped when a field changes shape or meaning.
Adding a field that leaves every existing field intact does not bump it.
Readers must tolerate added fields.
