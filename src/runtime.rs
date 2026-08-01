use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use opencode_rs::{
    ClientBuilder,
    types::{
        event::Event,
        message::{Part, PromptPart, PromptRequest},
        permission::{PermissionAction, PermissionRule, Ruleset},
        project::ModelRef,
        session::CreateSessionRequest,
    },
};
use serde::Serialize;
use tokio::time::timeout;

use crate::{config::Config, error::AppError};

#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    pub library_id: String,
    pub workdir: PathBuf,
    pub prompt: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRunResult {
    pub session_id: String,
    pub answer: String,
    pub tool_events: Vec<serde_json::Value>,
}

#[async_trait]
pub trait OpenCodeRuntime: Send + Sync {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError>;
}

#[derive(Clone)]
pub struct OpenCodeAgent {
    config: Arc<Config>,
}

impl OpenCodeAgent {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OpenCodeRuntime for OpenCodeAgent {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        let client = ClientBuilder::new()
            .base_url(&self.config.opencode_url)
            .directory(request.workdir.to_string_lossy())
            .timeout_secs(self.config.opencode_timeout_secs)
            .build()
            .map_err(|error| AppError::Runtime(error.to_string()))?;

        let session = client
            .sessions()
            .create(&CreateSessionRequest {
                title: Some(request.title.clone()),
                permission: Some(all_permissions()),
                ..Default::default()
            })
            .await
            .map_err(|error| AppError::Runtime(error.to_string()))?;

        let mut subscription = match client.subscribe_session(&session.id) {
            Ok(subscription) => subscription,
            Err(error) => {
                let _ = client.sessions().delete(&session.id).await;
                return Err(AppError::Runtime(error.to_string()));
            }
        };

        let model = parse_model(&self.config.opencode_model);
        let prompt = PromptRequest {
            parts: vec![PromptPart::Text {
                text: request.prompt,
                synthetic: None,
                ignored: None,
                metadata: None,
            }],
            message_id: None,
            model: Some(model),
            agent: None,
            no_reply: None,
            system: None,
            variant: None,
        };

        if let Err(error) = client.messages().prompt(&session.id, &prompt).await {
            let _ = client.sessions().delete(&session.id).await;
            return Err(AppError::Runtime(error.to_string()));
        }

        let collected = match timeout(
            Duration::from_secs(self.config.opencode_timeout_secs),
            collect_events(&mut subscription),
        )
        .await
        {
            Ok(Ok(collected)) => collected,
            Ok(Err(error)) => {
                let _ = client.sessions().delete(&session.id).await;
                return Err(error);
            }
            Err(_) => {
                let _ = client.sessions().delete(&session.id).await;
                return Err(AppError::Runtime("OpenCode session timed out".into()));
            }
        };

        let cleanup_result = client.sessions().delete(&session.id).await;
        if let Err(error) = cleanup_result {
            tracing::warn!(session_id = %session.id, error = %error, "failed to delete OpenCode session");
        }

        Ok(AgentRunResult {
            session_id: session.id,
            answer: collected.answer,
            tool_events: collected.tool_events,
        })
    }
}

#[derive(Default)]
struct CollectedEvents {
    answer: String,
    streamed_text: bool,
    fallback_text: Vec<String>,
    tool_events: Vec<serde_json::Value>,
}

impl CollectedEvents {
    fn finish(mut self) -> Self {
        if !self.streamed_text && !self.fallback_text.is_empty() {
            self.answer = self.fallback_text.join("\n");
        }
        self
    }
}

async fn collect_events(
    subscription: &mut opencode_rs::sse::SseSubscription<Event>,
) -> Result<CollectedEvents, AppError> {
    let mut collected = CollectedEvents::default();
    while let Some(event) = subscription.recv().await {
        match &event {
            Event::SessionIdle { .. } => break,
            Event::SessionError { properties } => {
                return Err(AppError::Runtime(format!(
                    "OpenCode session error: {:?}",
                    properties.error
                )));
            }
            Event::MessagePartDelta { properties } => {
                if let Some(delta) = properties.delta.as_deref() {
                    collected.answer.push_str(delta);
                    collected.streamed_text = true;
                }
            }
            Event::MessagePartUpdated { properties } => {
                if let Some(Part::Text { text, .. }) = properties.part.as_ref() {
                    collected.fallback_text.push(text.clone());
                }
            }
            Event::MessageUpdated { properties } => {
                if properties.info.role == "assistant"
                    && let Ok(value) = serde_json::to_value(&event)
                {
                    collected.tool_events.push(value);
                }
            }
            Event::CommandExecuted { .. }
            | Event::FileEdited { .. }
            | Event::McpToolsChanged { .. }
            | Event::PermissionAsked { .. } => {
                if let Ok(value) = serde_json::to_value(&event) {
                    collected.tool_events.push(value);
                }
            }
            _ => {}
        }
    }
    Ok(collected.finish())
}

fn parse_model(value: &str) -> ModelRef {
    let (provider, model) = value.split_once('/').unwrap_or(("opencode", value));
    ModelRef {
        provider_id: Some(provider.into()),
        model_id: Some(model.into()),
        variant: None,
        extra: serde_json::Value::Null,
    }
}

fn all_permissions() -> Ruleset {
    // Keep every OpenCode capability available, except the interactive
    // question tool: a headless knowledge service has no user-answer loop.
    // OpenCode evaluates the last matching rule, so the specific deny follows
    // the wildcard allow.
    vec![
        permission("*", "*", PermissionAction::Allow),
        permission("question", "*", PermissionAction::Deny),
    ]
}

fn permission(name: &str, pattern: &str, action: PermissionAction) -> PermissionRule {
    PermissionRule {
        permission: name.into(),
        pattern: pattern.into(),
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_allow_everything_except_question() {
        let rules = all_permissions();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], permission("*", "*", PermissionAction::Allow));
        assert_eq!(
            rules[1],
            permission("question", "*", PermissionAction::Deny)
        );
    }
}
