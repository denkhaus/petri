//! Reusable workflows keep GitHub semantics across execution boundaries.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acceptance::runs::stub_run_scripts;
use execution::{
    CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, ExecutionId, ExecutionObserver,
    InvocationId, SandboxBinding, host,
};
use runtime::ir::RunStatus;
use runtime::{engine, ir};
use serde_json::json;
use support::*;

#[tokio::test]
async fn sweep_transforms_nested_graphs_and_updates_call_digests() {
    let leaf = "on:\n  workflow_call: {}\njobs:\n  work:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 1\n";
    let middle =
        "on:\n  workflow_call: {}\njobs:\n  call:\n    uses: ./.github/workflows/leaf.yml\n";
    let caller = "on: push\njobs:\n  one:\n    uses: ./.github/workflows/middle.yml\n  two:\n    uses: ./.github/workflows/middle.yml\n";
    let run = lower_ok_with(
        caller,
        &files(&[
            (".github/workflows/leaf.yml", leaf),
            (".github/workflows/middle.yml", middle),
        ]),
    );
    let mut artifact = acceptance::Artifact {
        graph:    run.graph,
        children: run.children,
    };
    artifact.map_graphs(stub_run_scripts);
    assert_eq!(artifact.children.len(), 2, "shared children stay shared");
    let report = run_host(
        host::HostRun::new(artifact.graph).with_children(artifact.children),
        "sweep-nested-invocations",
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(report.lifecycle.iter().filter(|record| matches!(record.body,
        CoordinatorEvent::InvocationFinished { invocation, .. } if invocation != InvocationId::ROOT
    )).count(), 4);
}
use tokio::time;

#[tokio::test]
async fn nested_calls_keep_inputs_secrets_and_parameters() {
    let leaf = r#"
on:
  workflow_call:
    inputs:
      name: {type: string, default: default-name}
      enabled: {type: boolean, default: true}
    secrets:
      token: {required: true}
      optional: {required: false}
    outputs:
      value:
        value: ${{ jobs.work.outputs.value }}
env:
  INPUT_NAME: ${{ inputs.name }}
jobs:
  work:
    runs-on: ubuntu-latest
    outputs:
      value: ${{ steps.emit.outputs.value }}
    steps:
      - id: emit
        env:
          TOKEN: ${{ secrets.token }}
          OPTIONAL: ${{ secrets.optional }}
        run: |
          test "$GITHUB_REPOSITORY" = example/repo
          test "$INPUT_NAME" = default-name
          test "$TOKEN" = expected-token
          test -z "$OPTIONAL"
          echo "value=${{ inputs.name }}-${{ inputs.enabled }}-${{ github.repository }}" >> "$GITHUB_OUTPUT"
"#;
    let middle = r"
on:
  workflow_call:
    secrets:
      token: {required: true}
    outputs:
      value:
        value: ${{ jobs.inner.outputs.value }}
jobs:
  inner:
    uses: ./.github/workflows/leaf.yml
    secrets: inherit
";
    let caller = r"
on: push
jobs:
  call:
    uses: ./.github/workflows/middle.yml
    secrets:
      token: ${{ secrets.PARENT_TOKEN }}
  after:
    needs: call
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ needs.call.outputs.value }}
";
    let files = files(&[
        (".github/workflows/leaf.yml", leaf),
        (".github/workflows/middle.yml", middle),
    ]);
    let report =
        run_host_with_secrets(lower_ok_with(caller, &files), "nested-workflow-secrets", &[
            ("PARENT_TOKEN", "expected-token"),
        ])
        .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(log_lines(&report).contains(&"default-name-true-example/repo".into()));
    let children = report
        .lifecycle
        .iter()
        .filter(|record| {
            matches!(record.body, CoordinatorEvent::InvocationDeclared {
                call: Some(_),
                sandbox: SandboxBinding::Isolated,
                ..
            })
        })
        .count();
    assert_eq!(children, 2);
    assert!(
        !serde_json::to_string(&report.lifecycle)
            .unwrap()
            .contains("expected-token")
    );
}

#[tokio::test]
async fn caller_and_callee_matrices_keep_call_workspaces_isolated() {
    let callee = r#"
on:
  workflow_call:
    inputs:
      outer: {type: string, required: true}
jobs:
  build:
    strategy:
      matrix:
        inner: [x, y]
    runs-on: ubuntu-latest
    steps:
      - run: |
          test ! -e marker-${{ matrix.inner }}
          echo '${{ inputs.outer }}-${{ matrix.inner }}' > marker-${{ matrix.inner }}
          echo "leg=$(cat marker-${{ matrix.inner }})"
          echo "workspace=$PWD"
"#;
    let caller = r"
on: push
jobs:
  call:
    strategy:
      matrix:
        outer: [a, b]
    uses: ./.github/workflows/child.yml
    with:
      outer: ${{ matrix.outer }}
";
    let report = run_host(
        lower_ok_with(caller, &files(&[(".github/workflows/child.yml", callee)])),
        "nested-workflow-matrix",
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let lines = log_lines(&report);
    for leg in ["a-x", "a-y", "b-x", "b-y"] {
        assert!(lines.contains(&format!("leg={leg}")), "{lines:?}");
    }
    let workspaces: BTreeSet<_> = lines
        .iter()
        .filter(|line| line.starts_with("workspace="))
        .collect();
    assert_eq!(workspaces.len(), 2, "{lines:?}");
}

#[tokio::test]
async fn child_summary_preserves_success_failure_and_skipped() {
    for (label, condition, command, expected) in [
        ("success", "true", "exit 0", RunStatus::Success),
        ("failure", "true", "exit 1", RunStatus::Failed),
        ("skipped", "false", "exit 1", RunStatus::Success),
    ] {
        let callee = format!(
            "on:\n  workflow_call: {{}}\njobs:\n  work:\n    if: {condition}\n    runs-on: ubuntu-latest\n    steps:\n      - run: {command}\n"
        );
        let caller = r"
on: push
jobs:
  call:
    uses: ./.github/workflows/child.yml
  after:
    needs: call
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo result=${{ needs.call.result }}
";
        let report = run_host(
            lower_ok_with(caller, &files(&[(".github/workflows/child.yml", &callee)])),
            &format!("workflow-summary-{label}"),
        )
        .await;
        assert_eq!(report.status, expected, "{:?}", report.state.errors());
        assert!(log_lines(&report).contains(&format!("result={label}")));
        assert_eq!(
            report.state.run_context().node("call/done").unwrap().output["result"],
            json!(label)
        );
    }
}

#[tokio::test]
async fn matrix_call_fail_fast_cancels_running_children() {
    let callee = r"
on:
  workflow_call:
    inputs:
      leg: {type: string, required: true}
jobs:
  work:
    runs-on: ubuntu-latest
    steps:
      - run: |
          if [ '${{ inputs.leg }}' = fail ]; then sleep 1; exit 1; fi
          echo child-ready
          sleep 30
          echo child-leaked
";
    let caller = r"
on: push
jobs:
  call:
    strategy:
      fail-fast: true
      matrix:
        leg: [slow, fail]
    uses: ./.github/workflows/child.yml
    with:
      leg: ${{ matrix.leg }}
";
    let report = time::timeout(
        Duration::from_secs(10),
        run_host(
            lower_ok_with(caller, &files(&[(".github/workflows/child.yml", callee)])),
            "workflow-fail-fast",
        ),
    )
    .await
    .expect("fail-fast stops the slow child");
    assert_eq!(report.status, RunStatus::Failed);
    assert!(log_lines(&report).contains(&"child-ready".into()));
    assert!(!log_lines(&report).contains(&"child-leaked".into()));
    assert!(report.lifecycle.iter().any(|record| matches!(&record.body,
        CoordinatorEvent::InvocationFinished { invocation, result }
            if *invocation != InvocationId::ROOT && result.status == RunStatus::Cancelled
    )));
}

#[derive(Default)]
struct CancelOnChildLog {
    handle:      Mutex<Option<CoordinatorHandle>>,
    invocations: Mutex<BTreeMap<ExecutionId, InvocationId>>,
    finished:    Mutex<BTreeMap<InvocationId, RunStatus>>,
    root:        bool,
}

#[async_trait::async_trait]
impl ExecutionObserver for CancelOnChildLog {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &engine::EventRecord,
        _recorded_at: u64,
        _state: &engine::EngineState,
    ) {
        if let engine::Event::StepProgressRecorded {
            ev: ir::StepEvent::Log { line, .. },
            ..
        } = &record.event
            && line == "child-ready"
        {
            let handle = self.handle.lock().unwrap();
            let handle = handle
                .as_ref()
                .expect("the host installs its handle before starting the child");
            if self.root {
                handle.cancel_root();
            } else {
                handle.cancel(self.invocations.lock().unwrap()[&execution]);
            }
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        match &record.body {
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } => {
                self.invocations
                    .lock()
                    .unwrap()
                    .insert(*execution, *invocation);
            }
            CoordinatorEvent::InvocationFinished { invocation, result } => {
                self.finished
                    .lock()
                    .unwrap()
                    .insert(*invocation, result.status);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn cancelling_the_parent_or_child_settles_both_executions() {
    let callee = r"
on:
  workflow_call: {}
jobs:
  work:
    runs-on: ubuntu-latest
    steps:
      - run: echo child-ready && sleep 30
";
    let caller = r"
on: push
jobs:
  call:
    uses: ./.github/workflows/child.yml
  after:
    needs: call
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo result=${{ needs.call.result }}
";
    for root in [false, true] {
        let dir = testkit::RunDir::new(if root {
            "cancel-workflow-parent"
        } else {
            "cancel-workflow-child"
        });
        let mut run = lower_ok_with(caller, &files(&[(".github/workflows/child.yml", callee)]));
        run.graph = with_params(run.graph);
        let observer = Arc::new(CancelOnChildLog {
            root,
            ..Default::default()
        });
        let report = time::timeout(
            Duration::from_secs(10),
            host::run_configured(
                &runtime(dir.path()),
                run.observe(observer.clone()),
                |handle, _| *observer.handle.lock().unwrap() = Some(handle),
            ),
        )
        .await
        .expect("cancellation reaches the child")
        .expect("the run finishes");
        assert_eq!(
            report.status,
            if root {
                RunStatus::Cancelled
            } else {
                RunStatus::Success
            }
        );
        assert!(testkit::log_lines(&report).contains(&"result=cancelled".into()));
        let finished = observer.finished.lock().unwrap();
        assert_eq!(finished.len(), 2);
        assert_eq!(finished[&InvocationId::ROOT], report.status);
        assert!(
            finished
                .iter()
                .filter(|(id, _)| **id != InvocationId::ROOT)
                .all(|(_, status)| *status == RunStatus::Cancelled)
        );
    }
}
