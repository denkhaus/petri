//! A scripted model that keeps one script per session.
//!
//! Pebble's [`ScriptedProvider`] answers calls from one queue in arrival
//! order. A parent agent and the children it spawns are separate sessions on
//! separate tasks, and which of them reaches the provider first is up to the
//! scheduler, so a queue they share hands answers to whichever session asks
//! first. [`RoutedProvider`] gives every session its own [`ScriptedProvider`]
//! and picks the script from the request itself: a child's first user
//! message is the task its parent gave it, so a lane keyed on that task
//! answers only that child. The root session's requests, and any request
//! whose first user message names no task, go to the root lane.
//!
//! Non-streaming calls (compaction's summary call) route the same way; the
//! summary request's one user message is the rendered transcript, which
//! opens with the session's first prompt.
//!
//! Pebble's test support has no such router at the pinned revision; this is
//! the capability Petri would take from the library once it exists.

use std::sync::Arc;

use async_trait::async_trait;
use lithos_llm::Client;
use lithos_llm::adapter::{ProviderAdapter, ResolvedCall};
use lithos_llm::catalog::AdapterId;
use lithos_llm::client::ClientBuild;
use lithos_llm::types::{Error as LlmError, ErrorKind, Response, ResponseStream, Role};
use pebble_coding_agent::test_support::{ScriptedProvider, message_text, test_catalog};

/// A provider that answers each session from that session's own script.
#[derive(Debug)]
pub(crate) struct RoutedProvider {
    id:       AdapterId,
    root:     Arc<ScriptedProvider>,
    children: Vec<(String, Arc<ScriptedProvider>)>,
}

impl RoutedProvider {
    /// The root session's script.
    pub(crate) fn root(&self) -> &ScriptedProvider {
        &self.root
    }

    /// The script of the child whose task is `task`.
    ///
    /// # Panics
    ///
    /// Panics when no child lane was keyed on `task`, which is a mistake in
    /// the test.
    pub(crate) fn child(&self, task: &str) -> &ScriptedProvider {
        self.children
            .iter()
            .find(|(key, _)| key == task)
            .map_or_else(
                || panic!("no scripted child is keyed on {task:?}"),
                |(_, lane)| lane.as_ref(),
            )
    }

    /// The lane a request belongs to, from its first user message.
    fn lane_for(&self, call: &ResolvedCall) -> Result<&ScriptedProvider, LlmError> {
        let opening = call
            .request()
            .messages()
            .iter()
            .find(|message| message.role() == Role::User)
            .map(message_text)
            .unwrap_or_default();
        let matched: Vec<&(String, Arc<ScriptedProvider>)> = self
            .children
            .iter()
            .filter(|(task, _)| opening.contains(task.as_str()))
            .collect();
        match matched.as_slice() {
            [] => Ok(&self.root),
            [(_, lane)] => Ok(lane),
            many => {
                let tasks: Vec<&str> = many.iter().map(|(task, _)| task.as_str()).collect();
                Err(LlmError::new(
                    ErrorKind::Middleware,
                    format!(
                        "the request's first user message names {tasks:?}; key each child lane on text only its task has"
                    ),
                ))
            }
        }
    }
}

#[async_trait]
impl ProviderAdapter for RoutedProvider {
    fn id(&self) -> &AdapterId {
        &self.id
    }

    async fn complete(&self, call: &ResolvedCall) -> Result<Response, LlmError> {
        self.lane_for(call)?.complete(call).await
    }

    async fn stream(&self, call: &ResolvedCall) -> Result<ResponseStream, LlmError> {
        self.lane_for(call)?.stream(call).await
    }
}

/// A client whose root session answers from `root` and whose children answer
/// from the lane keyed on their task.
///
/// A child's key must be text that only that child's task contains: a
/// request matching two keys is answered with an error.
///
/// # Panics
///
/// Panics if the client cannot be built, which is a bug in the test support.
pub(crate) fn routed_client(
    root: ScriptedProvider,
    children: Vec<(&str, ScriptedProvider)>,
) -> (Client, Arc<RoutedProvider>) {
    let provider = Arc::new(RoutedProvider {
        id:       AdapterId::new("test-adapter"),
        root:     Arc::new(root),
        children: children
            .into_iter()
            .map(|(task, lane)| (task.to_owned(), Arc::new(lane)))
            .collect(),
    });
    let shared: Arc<dyn ProviderAdapter> = Arc::clone(&provider) as Arc<dyn ProviderAdapter>;
    let ClientBuild { client, .. } = Client::builder()
        .catalog(test_catalog())
        .adapter_arc("test", Arc::clone(&shared))
        .adapter_arc("bare", shared)
        .build()
        .expect("the routed client builds");
    (client, provider)
}
