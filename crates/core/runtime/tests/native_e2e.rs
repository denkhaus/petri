//! Handoff §7 test 7, the behavioural half: the cycle + XOR + `any` example
//! runs end to end on the standard runtime. The lowering half stays with the
//! frontend, in `crates/core/frontend-native/tests/native.rs` (which lowers
//! this same document).

use std::{env, fs, process};

use runtime::executor::Retention;
use runtime::frontend::native::load;
use runtime::ir::RunStatus;
use runtime::{RunOptions, Runtime};

const CYCLE_XOR_ANY: &str = r#"
name: poll-until-ready
nodes:
  start:
    run: echo "attempt=0" > "$CI_OUTPUT"
    next: poll
  poll:
    join: any
    budget: { max_firings: 5 }
    run: |
      n=$(( ${ATTEMPT:-0} + 1 ))
      echo "attempt=$n" > "$CI_OUTPUT"
      if [ "$n" -ge 3 ]; then echo "ready=true" >> "$CI_OUTPUT"; else echo "ready=false" >> "$CI_OUTPUT"; fi
    config:
      env:
        ATTEMPT: ${{ input.attempt }}
    select:
      - when: ${{ output.ready != 'true' }}
        to: poll
        back: true
      - to: done
  done:
    run: echo ready after ${{ input.attempt }} attempts
"#;

/// The same graph the lowering tests inspect, on the real engine and executor:
/// three generations of `poll`, then `done`. Replay is verified by the runtime.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "the lowering diagnostics explain a failed `expect` below, and stderr is the only sink a test binary has"
)]
async fn the_cycle_runs_end_to_end() {
    let lowered = load("test.yml", CYCLE_XOR_ANY);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    let graph = lowered.graph.expect("lowers");

    let dir = env::temp_dir().join(format!("petri-native-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut options = RunOptions::new(&dir);
    options.retention = Retention::Never;
    let rt = Runtime::standard().options(options);
    let report = rt.run(graph).await.expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let polls: Vec<u32> = report
        .state
        .history()
        .iter()
        .filter(|r| r.name == "poll")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(polls, vec![0, 1, 2], "three generations, then the exit arm");
    assert_eq!(
        report
            .state
            .history()
            .iter()
            .filter(|r| r.name == "done")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(&dir);
}
