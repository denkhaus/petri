//! The provider twins, served in-process on ephemeral loopback ports.
//!
//! Both twins run in strict fixture mode: a scenario file is loaded, unmatched
//! generation calls fail with `scenario_not_found`, and every request is
//! written to a JSONL request log the test reads back. A middleware layer
//! also keeps every request body, so a test can assert what reached the
//! provider boundary: model, reasoning settings, tool results.
//!
//! Scenarios carry a `namespace`: the fake credential the case hands the
//! binary. Two cases sharing one twin cannot consume each other's scripts.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use twin_anthropic::config::Config as AnthropicConfig;
use twin_openai::config::Config as OpenAiConfig;

/// Which provider a twin stands in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    OpenAi,
    Anthropic,
}

impl Provider {
    /// The `lithos-llm` provider id.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    /// The credential variable `lithos-llm` reads for this provider.
    pub(crate) fn credential_env(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
        }
    }

    /// The scenario matcher endpoint for a generation request.
    pub(crate) fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAi => "responses",
            Self::Anthropic => "messages",
        }
    }
}

/// One captured request: the bearer or api key it carried, and its body.
#[derive(Clone, Debug)]
pub(crate) struct Captured {
    pub(crate) credential: Option<String>,
    pub(crate) body:       Value,
}

/// One running twin.
pub(crate) struct Twin {
    pub(crate) provider: Provider,
    /// `http://127.0.0.1:<port>`, what a catalog layer's `base_url` takes.
    pub(crate) base_url: String,
    requests:            Arc<Mutex<Vec<Captured>>>,
    log_path:            PathBuf,
    task:                JoinHandle<()>,
}

impl Twin {
    /// Start a twin with `scenarios` loaded as its strict fixture. `dir`
    /// receives the scenario file and the request log.
    pub(crate) async fn start(provider: Provider, dir: &Path, scenarios: Vec<Value>) -> Self {
        fs::create_dir_all(dir).expect("twin dir");
        let scenarios_path = dir.join(format!("{}-scenarios.json", provider.id()));
        fs::write(
            &scenarios_path,
            serde_json::to_vec_pretty(&json!({ "scenarios": scenarios })).expect("scenarios"),
        )
        .expect("write scenarios");
        let log_path = dir.join(format!("{}-requests.jsonl", provider.id()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = match provider {
            Provider::OpenAi => {
                let mut config = OpenAiConfig::from_lookup(&|_| None).expect("twin-openai config");
                config.scenarios_path = Some(scenarios_path);
                config.request_log_path = Some(log_path.clone());
                config.allow_unmatched = false;
                config.require_auth = true;
                twin_openai::build_app_with_config(config).expect("twin-openai app")
            }
            Provider::Anthropic => {
                let mut config =
                    AnthropicConfig::from_lookup(&|_| None).expect("twin-anthropic config");
                config.scenarios_path = Some(scenarios_path);
                config.request_log_path = Some(log_path.clone());
                config.allow_unmatched = false;
                config.require_auth = true;
                twin_anthropic::build_app_with_config(config).expect("twin-anthropic app")
            }
        };
        let app = capture(app, requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind twin");
        let base_url = format!("http://{}", listener.local_addr().expect("twin address"));
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                panic!("twin server failed: {error}");
            }
        });
        Self {
            provider,
            base_url,
            requests,
            log_path,
            task,
        }
    }

    /// Every request body this twin saw, in arrival order.
    pub(crate) fn requests(&self) -> Vec<Captured> {
        self.requests.lock().expect("not poisoned").clone()
    }

    /// Request bodies carrying `credential`, in arrival order.
    pub(crate) fn requests_for(&self, credential: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request.credential.as_deref() == Some(credential))
            .map(|request| request.body)
            .collect()
    }

    /// The twin's own request log: one record per generation request, with
    /// the scenario id it matched (absent when nothing matched).
    pub(crate) fn request_log(&self) -> Vec<Value> {
        let text = fs::read_to_string(&self.log_path).unwrap_or_default();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("request log line"))
            .collect()
    }

    /// The scenario ids the request log says were consumed, in order.
    pub(crate) fn consumed(&self) -> Vec<String> {
        self.request_log()
            .iter()
            .filter_map(|record| record["scenario_id"].as_str().map(str::to_owned))
            .collect()
    }

    /// How many logged requests matched no scenario.
    pub(crate) fn unmatched(&self) -> usize {
        self.request_log()
            .iter()
            .filter(|record| record["scenario_id"].is_null())
            .count()
    }

    /// The catalog layer that points `lithos-llm` at this twin.
    pub(crate) fn catalog_layer(&self) -> String {
        format!(
            "schema_version = 1\n[providers.{}]\nbase_url = {:?}\n",
            self.provider.id(),
            self.base_url
        )
    }

    pub(crate) fn stop(self) {
        self.task.abort();
    }
}

impl Drop for Twin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Keep every request body beside the credential it carried.
fn capture(app: Router, requests: Arc<Mutex<Vec<Captured>>>) -> Router {
    app.layer(from_fn(move |request: Request, next: Next| {
        let requests = requests.clone();
        async move {
            let credential = request
                .headers()
                .get("x-api-key")
                .or_else(|| request.headers().get("authorization"))
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim_start_matches("Bearer ").to_owned());
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, 32 * 1024 * 1024)
                .await
                .expect("bounded request body");
            if let Ok(body) = serde_json::from_slice::<Value>(&bytes) {
                requests
                    .lock()
                    .expect("not poisoned")
                    .push(Captured { credential, body });
            }
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
    }))
}

/// A success scenario: one model turn, matched on the prompt or tool output
/// text the request carries.
pub(crate) fn scenario(
    provider: Provider,
    namespace: &str,
    id: &str,
    model: &str,
    input_contains: &str,
    script: Value,
) -> Value {
    let mut scenario = Map::new();
    scenario.insert("scenario_id".into(), json!(id));
    scenario.insert("namespace".into(), json!(namespace));
    scenario.insert(
        "matcher".into(),
        json!({
            "endpoint": provider.endpoint(),
            "model": model,
            "input_contains": input_contains,
        }),
    );
    scenario.insert("script".into(), script);
    Value::Object(scenario)
}

/// A plain text reply.
pub(crate) fn text(text: &str) -> Value {
    json!({
        "kind": "success",
        "response_text": text,
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

/// A reply that calls one tool.
pub(crate) fn tool_call(id: &str, name: &str, arguments: Value) -> Value {
    let mut call = Map::new();
    call.insert("id".into(), json!(id));
    call.insert("name".into(), json!(name));
    call.insert("arguments".into(), arguments);
    json!({
        "kind": "success",
        "tool_calls": [Value::Object(call)],
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

/// The shell tool each Pebble harness offers the twin's model: Codex's
/// `shell_command` for GPT-5.6, Claude 5's `Bash`.
pub(crate) fn shell_tool(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "shell_command",
        Provider::Anthropic => "Bash",
    }
}

/// The question tool each harness offers.
pub(crate) fn question_tool(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "request_user_input",
        Provider::Anthropic => "AskUserQuestion",
    }
}

/// The built-in catalog model each twin answers as.
pub(crate) fn model(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "gpt-5.6-sol",
        Provider::Anthropic => "claude-sonnet-5",
    }
}

/// The reasoning effort a request asked for, as each wire encodes it.
pub(crate) fn requested_effort(provider: Provider, body: &Value) -> Option<&str> {
    match provider {
        Provider::OpenAi => body["reasoning"]["effort"].as_str(),
        Provider::Anthropic => body["output_config"]["effort"].as_str(),
    }
}
