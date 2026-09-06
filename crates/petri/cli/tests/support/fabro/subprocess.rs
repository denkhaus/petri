//! Subprocess control for the shipped `petri` binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// A `petri run` invocation on a Fabro workflow.
pub(crate) struct Petri {
    workflow: PathBuf,
    run_dir:  PathBuf,
    inputs:   Vec<(String, String)>,
    timeout:  Duration,
}

/// What one `petri run` left behind.
pub(crate) struct RunOutput {
    pub(crate) run_dir: PathBuf,
    pub(crate) output:  Output,
}

impl Petri {
    pub(crate) fn run_workflow(workflow: &Path, run_dir: &Path) -> Self {
        Self {
            workflow: workflow.to_path_buf(),
            run_dir:  run_dir.to_path_buf(),
            inputs:   Vec::new(),
            timeout:  Duration::from_secs(120),
        }
    }

    /// A `--input KEY=VALUE` the format renders before lowering.
    #[must_use]
    pub(crate) fn input(mut self, key: &str, value: impl Into<String>) -> Self {
        self.inputs.push((key.to_string(), value.into()));
        self
    }

    /// Run to completion and collect the output. The process is killed when
    /// it outlives the timeout, so a wedged run fails the test instead of
    /// hanging it.
    ///
    /// # Panics
    ///
    /// Panics when the binary cannot be spawned.
    pub(crate) fn run(self) -> RunOutput {
        let mut command = Command::new(env!("CARGO_BIN_EXE_petri"));
        command.arg("run").arg("--run-dir").arg(&self.run_dir);
        for (key, value) in &self.inputs {
            command.arg("--input").arg(format!("{key}={value}"));
        }
        command.arg(&self.workflow);
        let output = run_with_timeout(command, self.timeout);
        RunOutput {
            run_dir: self.run_dir,
            output,
        }
    }
}

impl RunOutput {
    pub(crate) fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    pub(crate) fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    pub(crate) fn success(&self) -> bool {
        self.output.status.success()
    }

    /// The `run: <status>` line the CLI prints last.
    pub(crate) fn run_status(&self) -> Option<String> {
        self.stderr()
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("run: ").map(str::to_string))
    }

    /// The `  <status> <node>` history lines the CLI prints after a run, in
    /// order.
    pub(crate) fn node_history(&self) -> Vec<(String, String)> {
        self.stderr()
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("  ")?;
                let (status, node) = rest.split_once(' ')?;
                matches!(status, "success" | "failure" | "partial" | "cancelled")
                    .then(|| (status.to_string(), node.to_string()))
            })
            .collect()
    }

    /// The echoed output lines of one node (`[<node>#<firing>] <line>` on
    /// stderr), joined.
    pub(crate) fn echoed(&self, node: &str) -> String {
        let prefix = format!("[{node}#");
        let mut text = String::new();
        for line in self.stderr().lines() {
            let Some(rest) = line.strip_prefix(&prefix) else {
                continue;
            };
            let Some(end) = rest.find("] ") else {
                continue;
            };
            text.push_str(&rest[end + 2..]);
            text.push('\n');
        }
        text
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    use std::io::Read as _;
    use std::process::Stdio;
    use std::thread;
    use std::time::Instant;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("the petri binary spawns");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child can be polled") {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            break child.wait().expect("a killed child can be reaped");
        }
        thread::sleep(Duration::from_millis(50));
    };
    Output {
        status,
        stdout: out.join().expect("the stdout reader finishes"),
        stderr: err.join().expect("the stderr reader finishes"),
    }
}
