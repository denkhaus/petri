//! Step kinds: the semantics of running one step.
//!
//! A step kind knows nothing about host-versus-Docker. It receives an
//! [`ExecEnv`](executor::ExecEnv) capability and uses it, which is why the process
//! step is written once and runs either way.

pub mod ctx;
pub mod outputs;
pub mod process;

pub use ctx::{RunnerRegistry, StepCtx, StepRunner};
pub use outputs::{BAD_OUTPUT_CLASS, OutputError, parse as parse_outputs};
pub use process::{
    OUTPUT_ENV, PROCESS_KIND, ProcessConfig, ProcessStep, SECRET_MISPLACED_CLASS, Shell, SoftFail,
    ValueOrSecretRef,
};
