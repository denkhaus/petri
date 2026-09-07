//! The model client Petri ships, against a twin: its own request retries and
//! its call budget are what the distribution says they are, before any
//! fallback decision is taken on top of them.

mod support;

use std::sync::Arc;
use std::time::Duration;

use lithos_llm::credentials::{Credentials, SecretValue, StaticCredentials};
use lithos_llm::types::{ErrorKind, Request};
use petri::{LlmClientConfig, build_llm_client};
use support::fabro::failures::{error, hang, repeated};
use support::fabro::twins::{Provider, Twin, model, scenario};

fn config(twin: &Twin, retry_attempts: u32, timeout: Option<Duration>) -> LlmClientConfig {
    let credentials = StaticCredentials::new().with(
        twin.provider.id(),
        Credentials::bearer(SecretValue::new("suite")),
    );
    LlmClientConfig {
        credentials: Some(Arc::new(credentials)),
        layers: vec![("twin".into(), twin.catalog_layer())],
        providers: Some(vec![twin.provider.id().to_owned()]),
        retry_attempts,
        timeout,
    }
}

fn request(model: &str) -> Request {
    Request::builder()
        .model(model)
        .user("Say hello")
        .build()
        .expect("request")
}

/// A retryable failure is sent again up to the budget; the error that comes
/// back keeps its kind.
#[tokio::test]
async fn the_client_retries_a_retryable_failure_up_to_its_budget() {
    let dir = std::env::temp_dir().join(format!("petri-llm-client-retry-{}", testkit::unique_id()));
    let twin = Twin::start(Provider::OpenAi, &dir, vec![repeated(
        scenario(
            Provider::OpenAi,
            "suite",
            "flaky",
            model(Provider::OpenAi),
            "Say hello",
            error(503, "server_error", "service_unavailable", "flaky"),
        ),
        5,
    )])
    .await;
    let client = build_llm_client(&config(&twin, 3, None)).expect("client");
    let failure = client
        .complete(request("openai/gpt-5.6-sol"))
        .await
        .expect_err("the twin only fails");
    assert_eq!(failure.kind(), ErrorKind::Server, "{failure}");
    assert_eq!(twin.consumed(), ["flaky", "flaky", "flaky"], "three sends");
    let client = build_llm_client(&config(&twin, 1, None)).expect("client");
    let _ = client.complete(request("openai/gpt-5.6-sol")).await;
    assert_eq!(twin.consumed().len(), 4, "one send with retries off");
    // The streaming path Pebble consumes retries the same way; the black box
    // case `client_retries_are_spent_before_the_chain_advances` proves it
    // through the binary.
    twin.stop();
}

/// A call that outlives the budget fails with the timeout kind.
#[tokio::test]
async fn the_client_budget_ends_a_hanging_call_with_a_timeout() {
    let dir =
        std::env::temp_dir().join(format!("petri-llm-client-timeout-{}", testkit::unique_id()));
    let twin = Twin::start(Provider::OpenAi, &dir, vec![scenario(
        Provider::OpenAi,
        "suite",
        "hangs",
        model(Provider::OpenAi),
        "Say hello",
        hang(),
    )])
    .await;
    let client =
        build_llm_client(&config(&twin, 1, Some(Duration::from_millis(500)))).expect("client");
    let started = std::time::Instant::now();
    let failure = tokio::time::timeout(
        Duration::from_secs(10),
        client.stream(request("openai/gpt-5.6-sol")),
    )
    .await
    .expect("the budget fires before the test's own limit")
    .err()
    .map(|e| e.kind());
    assert_eq!(
        failure,
        Some(ErrorKind::Timeout),
        "after {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    twin.stop();
}
