//! Each holder of a shared sandbox keeps its own scope environment.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use executor::{
    AcquireContext, ExecEnv, Executor, ProcessSpec, Retention, SandboxLeaseId, ScopeOutcome,
    ScopeSpec,
};
use executor_sandbox::RoutingExecutor;
use ir::{RuntimeSpec, RuntimeTarget, ScopeId};
use testkit::{RunDir, is_docker_ready};
use tokio::time::timeout;

async fn values(env: &dyn ExecEnv) -> String {
    let spec = ProcessSpec::new("sh", &[
        "-c",
        "printf '%s|%s|%s\\n' \"$SCOPE_VALUE\" \"$OPTION_VALUE\" \"$PROCESS_VALUE\"",
    ])
    .with_env(BTreeMap::from([("PROCESS_VALUE".into(), "process".into())]));
    let mut process = env.spawn(spec).await.expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut output = String::new();
    while let Some(line) = lines.recv().await {
        output.push_str(&line.line);
    }
    assert!(process.wait().await.expect("wait").is_success());
    output
}

#[tokio::test]
async fn inherited_holders_keep_scope_options_and_process_environment_precedence() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("shared-sandbox-environment");
    let router = RoutingExecutor::local(dir.path(), Retention::Never).with_run_id(dir.run_id());
    let mut runtime = RuntimeSpec::container("alpine:3.20");
    if let RuntimeTarget::Container { options, .. } = &mut runtime.target {
        options.env = vec![
            ("OPTION_VALUE".into(), "option".into()),
            ("PROCESS_VALUE".into(), "option".into()),
        ];
    }
    let parent = ScopeSpec::new(ScopeId::new(0), "parent")
        .with_runtime(runtime.clone())
        .with_env(BTreeMap::from([("SCOPE_VALUE".into(), "parent".into())]));
    let child = ScopeSpec::new(ScopeId::new(1), "child")
        .with_workspace_id(parent.workspace_id.clone())
        .with_runtime(runtime)
        .with_env(BTreeMap::from([
            ("SCOPE_VALUE".into(), "child".into()),
            ("OPTION_VALUE".into(), "child".into()),
            ("PROCESS_VALUE".into(), "child".into()),
        ]));
    let lease = SandboxLeaseId::new(0);
    let ctx = AcquireContext::bare().with_lease(lease);
    let parent = router.acquire(&parent, &ctx).await.expect("parent");
    let mut waiting = parent
        .exec()
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            "echo waiting; while [ ! -f child-ready ]; do sleep 0.05; done",
        ]))
        .await
        .expect("parent process");
    let mut waiting_lines = waiting.lines().expect("parent output");
    assert_eq!(
        timeout(Duration::from_secs(10), waiting_lines.recv())
            .await
            .unwrap()
            .unwrap()
            .line,
        "waiting"
    );
    let name = dir.sandbox_name(lease.raw());
    let sandbox_id = testkit::container_id(&name).await.expect("parent sandbox");
    let child = router.acquire(&child, &ctx).await.expect("child");
    assert_eq!(
        testkit::container_id(&name).await.as_deref(),
        Some(sandbox_id.as_str())
    );
    let child_env = child.exec();
    child_env
        .write_file(Path::new("child-ready"), b"ready")
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(10), waiting.wait())
            .await
            .unwrap()
            .unwrap()
            .is_success(),
        "child acquisition must not stop the parent's process"
    );
    assert_eq!(
        child_env.ambient_env("SCOPE_VALUE").as_deref(),
        Some("child")
    );
    assert_eq!(
        child_env.ambient_env("OPTION_VALUE").as_deref(),
        Some("option")
    );
    assert!(child_env.ambient_env("PATH").is_some());
    assert_eq!(values(child_env.as_ref()).await, "child|option|process");
    assert_eq!(
        values(parent.exec().as_ref()).await,
        "parent|option|process"
    );

    assert!(
        router
            .release(child, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert!(
        router
            .release(parent, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert!(
        router
            .release_lease(lease, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}
