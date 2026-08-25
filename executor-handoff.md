# Handoff: Executor Package (02)

**Follows:** dda482c (core semantics complete: ir-design.md + core-gaps-handoff.md)
**Purpose:** Everything between `Command::StartStep(ResolvedFiring)` and real
processes: the driver loop, the `Executor` trait with host and Docker
implementations, the process StepKind, scope environments, and secret handling.
After this package, the native loop example and the GHA-shaped example run end to
end on real processes.

Read §4 (cancellation) first — it is the part of this package where unstated
requirements become bugs, and it constrains the design of §1–§3.

---

## 0. Scope

**In:** driver event loop; `Executor` trait; host-process executor; Docker
executor; process StepKind; scope environment lifecycle; secret injection and log
masking; the acceptance battery in §7.

**Out (do not build):** GHA action shims and the JS action host (package 03);
artifact and cache stores (03); remote/distributed executors, Windows, service
containers, cpu/mem resource limits (v2). The `LogSink` here writes to the run
directory and stdout; fancier sinks come later.

---

## 1. Layering

Three layers, one rule each:

- **Driver** — owns all IO scheduling. Single consumer of core commands, single
  producer of external events. No policy decisions; it translates.
- **Executor** — owns *environments* (workspaces, containers). Knows nothing about
  steps' semantics.
- **StepKind** — owns step semantics. Knows nothing about host-vs-Docker; it
  receives an environment capability and uses it.

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    /// Materialize the environment for one scope instance.
    async fn acquire(&self, scope: &ScopeSpec) -> Result<EnvHandle, EnvError>;

    /// Tear down. Idempotent; must never fail the run (best effort + report).
    async fn release(&self, env: EnvHandle) -> ReleaseReport;
}

/// Capability handed to StepKinds via StepCtx. Abstracts host vs container.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError>;
    fn workspace(&self) -> &Path;          // host-visible path (bind-mounted for Docker)
}

pub struct ProcessSpec {
    pub program: SmolStr,                  // e.g. "bash"
    pub args: Vec<SmolStr>,                // e.g. ["-eo", "pipefail", "-c", script]
    pub env: BTreeMap<SmolStr, SmolStr>,
    pub cwd: RelativePath,                 // relative to workspace
}

#[async_trait]
pub trait ProcessHandle: Send {
    fn stdout(&mut self) -> LineStream;    // line-buffered, stream-tagged
    fn stderr(&mut self) -> LineStream;
    async fn wait(&mut self) -> ExitStatus;
    /// Signal the process GROUP (see §4). Idempotent.
    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError>;
}
```

The process StepKind is written once against `ExecEnv` and never mentions Docker.

---

## 2. Driver loop contract

1. **Append-then-apply.** Every event is appended to the v2 log (with its
   `EventSource`) before `apply` sees it. All events produced in this package —
   `StepStarted` acknowledgements are core-side; `StepProgress`, `StepFinished`,
   `RetryElapsed`, timeout-induced finishes, `CancelRequested` — are
   `EventSource::External`. Replay feeds back External only; the core regenerates
   its own (this is the determinism check from core package #9).
2. **Total order is arrival order.** Concurrent step futures funnel through one
   mpsc; the append order is canonical. Replay byte-identity holds because all
   wall-clock nondeterminism (completion order, retry timing) is captured in
   External events.
3. **Timers.** The driver arms `Budget.timeout` per attempt at `StartStep`
   dispatch. On expiry it runs the §4 cancellation ladder and reports
   `StepFinished` with `Status::TimedOut`. `ScheduleRetry` → driver applies
   jitter → sleeps → emits `RetryElapsed`.
4. **Hard deadline on cancellation.** After delivering `Control::Cancel` to a
   StepKind, the driver enforces `grace + 5s`; if the step future hasn't returned,
   the driver aborts the task and synthesizes `Status::Cancelled` with failure
   class `"cancel_forced"`. No StepKind, however buggy, may wedge a run.
5. **Command dispatch:** `AcquireScope` → `Executor::acquire` (failure: see §5.3);
   `ReleaseScope` → `Executor::release` (never fails the run); `DeliverControl` →
   the step's `ControlRx`.

---

## 3. Process StepKind

### 3.1 Config

```rust
pub struct ProcessConfig {
    pub run: String,                        // script text
    pub shell: Shell,                       // Bash (default) | Sh
    pub env: BTreeMap<SmolStr, ValueOrSecretRef>,
    pub working_dir: Option<RelativePath>,  // default: workspace root
    pub soft_fail: SoftFail,                // Off (default) | All | ExitStatuses(Vec<i32>)
}
```

Bash invocation: `bash -eo pipefail -c <run>` (match GHA defaults). No step-level
timeout — `Budget.timeout` on the node governs; the GHA frontend maps
`timeout-minutes` onto it.

### 3.2 Outcome mapping

| Exit | Status |
|---|---|
| 0 | `Success` |
| N ≠ 0, `soft_fail` matches | `PartialSuccess { underlying: class "exit_status:N" }` |
| N ≠ 0 otherwise | `Failure { class: "exit_status:N" }` |
| killed by signal S (not by us) | `Failure { class: "signal:S" }` |
| killed by our cancel ladder | `Cancelled` / `TimedOut` (per §4.3 race rule) |

### 3.3 Structured outputs (outputs-file protocol)

The step gets `CI_OUTPUT=<workspace>/.ci/out/<firing>.env` in its environment.
After exit, the StepKind parses it — `key=value` lines plus the GHA heredoc form
(`key<<DELIM … DELIM`) — into `Outcome.output` as an object (exit status is
included as `output.exit_status`). The GHA shim layer will alias `GITHUB_OUTPUT`
to the same file; parse failures are a step `Failure { class: "bad_output_file" }`,
not a panic.

### 3.4 Log capture

stdout/stderr are read concurrently, line-buffered, and emitted as
`StepEvent::Log { stream, line }` in arrival order. Lines are capped at 64 KiB
with a truncation marker. Capture continues through cancellation until both
streams close — cancelled steps' final output must not be lost.

---

## 4. Cancellation and timeout (read first)

### 4.1 Process groups, not processes

Host: spawn with `setpgid(0,0)` (own process group); every signal in this package
is sent with `killpg`, never `kill` on the child pid. A `run:` script that
launches background children must die as a unit. Known limitation to document,
not solve: double-forking daemons that re-parent to init escape the group on the
host executor (Docker's PID namespace contains them; that's the mitigation).

Docker: the container runs with `--init` (zombie reaping). Steps run via
`docker exec` as their own in-container process group. **`docker kill` signals
PID 1 only — it does NOT reach exec'd processes.** Step-level cancellation is
therefore `docker exec <container> kill -<SIG> -- -<PGID>`; container-level kill
is reserved for scope release. Getting this wrong is the classic bug; there is an
acceptance test aimed at it.

### 4.2 The ladder

One ladder for both cancel and timeout: `SIGTERM` to the group → grace period
(default **10s**, per-scope configurable) → `SIGKILL` to the group. The ladder is
idempotent: repeated cancels join the in-flight ladder rather than restarting it.

### 4.3 Races and terminal-event discipline

- Exactly one terminal `StepFinished` per firing, always — cancelled, timed out,
  crashed, or wedged (via the §2.4 hard deadline).
- Cancel-vs-natural-exit race: first terminal wins. If the process exits before
  the ladder's first signal lands, the outcome is the natural one and the cancel
  is a no-op. If the ladder has signalled, the outcome is
  `Cancelled`/`TimedOut` even if the exit status arrives looking ordinary.
- Timeout and cancel racing each other: `TimedOut` if the timer fired first,
  `Cancelled` otherwise — decided by driver arrival order, which the log then
  makes canonical.

### 4.4 Post-conditions

After any cancellation: both log streams drained to close; the outputs file is
still parsed if present (a TERM-trapping step may write outputs on the way out —
they are recorded in the finish record but, as a discarded/cancelled attempt's
data, follow the same visibility rules as any non-final outcome); scope release
still runs per normal scope-hold rules. Cancellation must never leak a workspace,
a container, or a process group.

---

## 5. Scope environments

### 5.1 Host executor

Per scope instance: workspace at `<run_dir>/scopes/<scope_instance>/work`, env
from the resolved `Scope.env`. Release deletes the workspace, except: retention
default is **keep on failure, delete on success**, overridable
(`keep_workspaces: always|on_failure|never`).

### 5.2 Docker executor

`acquire`: pull policy `if-not-present`; create container with `--init`, the
scope's image, the workspace **bind-mounted** from the host run dir (uniform
artifact/log handling), long-lived init command (`sleep infinity`); start.
Steps: `docker exec` per §4.1. `release`: TERM the container, grace, then
`rm -f`. Linux native; macOS via Docker Desktop — both in CI for this package.

### 5.3 Acquire failure

`acquire` failure (bad image, pull failure, daemon down) fails every pending
firing in that scope instance with `Failure { class: "env_acquire" }` — routable
and retryable like any failure, never a run abort. The event is External
(`StepFinished` per affected firing; no special event type).

### 5.4 Restated from core package (unchanged, now enforced for real)

Release is irreversible; re-entry after release acquires a *fresh* environment.
The re-entry lint warning from core item 6 (with the All-join suppression) is the
compile-time counterpart; this package makes the runtime behavior real.

---

## 6. Secrets

### 6.1 Secret references — amendment to the ResolvedFiring invariant

Secrets must never enter the event log, and `ResolvedFiring` is serialized into
`StartStep` records. Therefore secrets are **not** resolved at firing resolution.
A new terminal placeholder form `{"$secret": "NAME"}` is *permitted* by
`ResolvedFiring::new` — the constructor invariant becomes: **no unresolved
expression placeholders; secret references are allowed and are the only
non-literal form that may cross the boundary.** Resolution happens at
`spawn`-time in the StepKind/driver via `SecretProvider::resolve(name)`, directly
into the child's environment. Secret refs are valid only in
`ProcessConfig.env`-shaped positions; a `$secret` anywhere else in config is
`Failure { class: "secret_misplaced" }`. Secrets are not in `EvalEnv` — guards
cannot read them, by construction.

### 6.2 Masking

The `LogSink` holds the set of resolved secret values for the run (values with
< 6 characters are exempt, GHA-style, to avoid masking noise). Masking is
exact-match replacement with `***`, applied:

- per line on `StepEvent::Log` before persistence or streaming (multiline
  secrets: each line of the secret is masked independently);
- to every string value in `Outcome.output` and `Outcome.context_updates`
  before the finish record is appended.

Masking happens *before* the log append — the persisted log is post-mask.
Encoded variants (base64, urlencoded) are a documented v2 gap, not in scope.

---

## 7. Acceptance battery

1. **E2E native loop** — the sequential for_each example, real bash steps, host
   executor, Linux + macOS. Replay of the finished log is byte-identical
   (External-only feed).
2. **E2E GHA-shaped** — the `tests/gha.rs` graph end to end: `needs` fan-out/join,
   matrix expansion, `continue-on-error` → `soft_fail` → `PartialSuccess` routing.
3. **Group kill** — step spawns a background grandchild (`sleep 300 &`); cancel;
   assert the grandchild is dead. Host and Docker (the Docker variant is the
   §4.1 exec-pgid test).
4. **TERM honored** — step traps TERM, writes a file, exits 0 within grace;
   outcome `Cancelled`; the file exists; outputs file was parsed.
5. **TERM ignored** — step traps and ignores TERM; KILL lands after grace;
   outcome `Cancelled`, class records the escalation.
6. **Timeout** — budget expiry produces `TimedOut` via the same ladder; a timeout
   racing a natural exit resolves per §4.3 deterministically under replay.
7. **Acquire failure** — bad image name: all firings in scope fail
   `"env_acquire"`; an `if: failure()` cleanup path routes; run completes.
8. **Secrets** — step `echo`s a secret: log shows `***`; secret round-trips into
   `output` and is masked there; the raw event log contains only the `$secret`
   ref, verified by grepping the log bytes for the value.
9. **Wedged StepKind** — a test StepKind that ignores `Control::Cancel` and never
   returns: driver hard-deadline fires, `"cancel_forced"`, run completes.
10. **Scope retention** — failure keeps the workspace, success deletes it;
    Docker release leaves no container behind (assert via `docker ps -a`).

## 8. Defaults table

| Knob | Default |
|---|---|
| Cancel grace | 10s (per-scope override) |
| Driver hard deadline | grace + 5s |
| Log line cap | 64 KiB + truncation marker |
| Workspace retention | keep on failure |
| Docker pull policy | if-not-present |
| Secret min length for masking | 6 |
| Shell | `bash -eo pipefail` |

Departures from this document go in the README with reasoning, same as the last
two packages — that section is the most useful part of your report.
