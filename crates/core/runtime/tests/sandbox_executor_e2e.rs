//! The same native cycle as `native_e2e`, driven through a `RoutingExecutor`
//! whose container side is never touched: proof that the whole driver and
//! coordinator stack runs a graph over the router and the native host
//! executor, with byte-identical replay, and no plugin launched.

use std::{env, fs, process};

use executor_sandbox::RoutingExecutor;
use runtime::executor::Retention;
use runtime::frontend::native::load;
use runtime::ir::RunStatus;
use runtime::{RunOptions, Runtime};

const CYCLE: &str = r#"
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

#[tokio::test]
async fn the_cycle_runs_end_to_end_over_the_sandbox_adapter() {
    let lowered = load("test.yml", CYCLE);
    let graph = lowered.graph.expect("lowers");

    let dir = env::temp_dir().join(format!("petri-sandbox-native-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut options = RunOptions::new(&dir);
    options.retention = Retention::Never;

    let executor = RoutingExecutor::local(dir.clone(), Retention::Never);

    let rt = Runtime::standard().options(options).executor(executor);
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
    let _ = fs::remove_dir_all(&dir);
}
