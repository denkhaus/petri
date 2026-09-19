# Forking a stored run

`execution::host::fork_from` seeds a new run from a stored run's records up
to a position. The new run holds the source's state at that position: the
same graphs, the run context as it stood after the position's firing was
routed, the same invocation tree as far as it had reached, and a declaration
that names where it came from. The host then continues the new run with
`host::resume_configured`, as it would continue a crashed run.

Fabro builds rewind, fork and retry over this one entry point. Fabro keeps a
checkpoint record that ties a Petri position `(execution, firing)` to a Git
commit; a fork restores that commit into the new run's workspace from the
`scope_acquired` hook, before the first attempt after the position runs.

## Design

### The position

A position names a firing of one of the source's root-invocation executions:
`ForkPosition { execution, firing }`. The firing must have a `routing.resolved`
record in that execution's engine log. It is the last firing whose finish and
routing the fork keeps.

A position inside a child invocation is refused with
`ForkError::PositionInChild`. The parent firing that called the child is live
at any position inside it, so the fork could only drop the child and rerun
the call; the first version does not try to be clever about that.

### The engine log is truncated, not re-recorded

The position's execution keeps a prefix of its source engine log: every
record through the core records that the position firing's `routing.resolved`
produced (`route.applied`, `token.emitted`). The prefix ends at an apply
boundary, so `engine::verify_replay` reproduces it exactly, and every firing
after the position, whether started or only admitted, is gone.

With `ForkOptions::rerun_last`, the prefix ends just before the position
firing's first record instead (its admission, or a note before it). The
firing then exists in the replayed state, waiting for admission, and the
fork's resume admits and runs it again from its first attempt.

Truncation is prefix-based. A firing that ran concurrently with the position
firing and finished later in the log is cut with everything else after the
position and runs again in the fork.

The records are copied as stored, with their original recording times. The
alternative, replaying the source through a new coordinator and re-recording
the events, would produce the same records with new times and nothing else;
copying keeps the export rule (a record's own event carries the stored line)
and the time the event happened.

Every other kept execution's log is copied whole.

### What the fork's coordinator log holds

The fork's coordinator log is written fresh, re-sequenced from zero:

1. `run.started`, the fork's own declaration: the new key, the source's
   middleware chain, and `forked_from` (the source key, the position, and
   whether the position's firing runs again). This is the one record the fork
   invents.
2. `graph.registered` for every graph the source registered, in the source's
   order. The blobs are copied by digest, so the digests are the same.
3. The source's `invocation.declared`, `execution.declared`,
   `execution.finished`, `invocation.finished` and
   `invocation.cancel.requested` records for the kept part of the tree, in
   the source's order, with their original recording times.

The kept part of the tree is the root invocation with its executions up to
and including the position's, and every child invocation that is finished
and was called from a kept execution by a firing whose finish the fork keeps.
The position execution's own finish is never kept: it is the execution the
fork continues. Later root executions are dropped. A child called from a
firing after the position, or from a firing still live at it, is dropped with
its executions; the resumed parent firing calls it again. A child that had
not finished at the position is dropped for the same reason.

The fork's log does not carry the source's pause controls, run-level notes,
`scope.released` records or `run.finished`: they describe the source's own
sandboxes and end.

### Sandboxes are not inherited

The fork's resource log starts empty. When the fork resumes, the position
execution's held scopes are acquired again under fresh leases in the new run,
the executor makes a fresh workspace, and the `scope_acquired` hook runs before
any attempt, which is where a host restores the files it wants there. The
copied engine log names the source's lease in its `scope.acquired` record;
the resumed execution records its own.

A kept child that shared its caller's sandbox names a source lease in its
declaration. The coordinator resolves inherited leases at resume only for
invocations that have not finished, since a finished invocation is never
dispatched again.

### Versions

The run declaration gained the optional `forked_from`. The run format moves
from 6 to 7 and a version 6 run is refused, as every format change is. The
public event contract stays at version 4: `run.started` carries the field
unchanged under `record`, and a stream without it is not a fork. The inspect
document gains `forked_from` beside `run_key` without a version bump.

### The result

`ForkedRun` names the new run's key, its `ForkOrigin` (the source key, the
position and `rerun_last`) and the root graph's digest.

## Continuing a fork

The new run is a stored run whose position execution has not finished. Any
host continues it with `host::resume_configured` under the same middleware
list the source ran with, and `petri resume --run-dir` continues one written
to a run directory. `verify_export` and `replay_run` hold over the fork's
records before and after it runs; `petri inspect` reports `forked_from`.

## Tests

`crates/petri/lib/tests/fork.rs` covers the fork through the embedding
boundary: a three-stage command run forked after its first stage, a fork at
the last position with and without `rerun_last`, a fork after a parallel
fan-in and one before it, a position inside a branch refused, a fork at a
failed firing that keeps the failure route, and a fork of a run in a host's
own store (`MemoryRunStore`) under keys the host named.
`crates/petri/cli/tests/inspect_cli.rs` inspects a fork with the shipped
binary and continues it with `petri resume`.
