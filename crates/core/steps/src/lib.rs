//! Step kinds: the semantics of running one step, and the one registry.
//!
//! A step kind knows nothing about where a process runs. It receives an
//! [`ExecEnv`](executor::ExecEnv) capability and uses it, which is why the
//! process step is written once and runs either way.

mod caps;
mod ctx;
mod noop;
mod outputs;
mod process;

pub use caps::{CAPABILITY_UNAVAILABLE_CLASS, Capabilities, CapabilitiesBuilder};
pub use ctx::{Registry, Step, StepCtx, StepFailure, StepRunner};
pub use noop::{NOOP_KIND, NoopStep};
pub use outputs::{BAD_OUTPUT_CLASS, OutputError, parse as parse_outputs};
pub use process::{
    Ending, PROCESS_KIND, ProcessConfig, ProcessStep, SECRET_MISPLACED_CLASS,
    SECRET_UNAVAILABLE_CLASS, Shell, SoftFail, ValueOrSecretRef, WORKSPACE_CLASS,
    check_misplaced_secret, ending_outcome, ladder, stringify,
};
