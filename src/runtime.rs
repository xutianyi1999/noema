use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use opencode_rs::{
    Client, ClientBuilder,
    types::{
        event::{Event, MessagePartEventProps},
        message::{Part, PromptPart, PromptRequest},
        permission::{PermissionAction, PermissionRule, Ruleset},
        project::ModelRef,
        session::CreateSessionRequest,
    },
};
use tokio::time::{Instant, timeout_at};

use crate::{config::Config, error::AppError, transcript::Transcript};

#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    pub library_id: String,
    /// OpenCode session directory and the durable content-library project
    /// root. Every Noema session for one library uses this same directory.
    pub workdir: PathBuf,
    pub prompt: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct AgentRunResult {
    pub session_id: String,
    pub answer: String,
    pub tool_events: Vec<serde_json::Value>,
}

/// Ingestion and maintenance create one retained session per job. Queries may
/// instead resume a successful session from the same library by passing its
/// `session_id`.
/// The trait is generic (static dispatch) rather than dyn-dispatched so it can
/// use native async-trait support (`async fn` in traits is not dyn-compatible
/// on stable Rust). The RPITIT signature pins the future as `Send` so services
/// can spawn and serve it across threads; implementations may still be written
/// with `async fn`.
pub trait OpenCodeRuntime: Send + Sync + 'static {
    fn run_new_session(
        &self,
        request: AgentRunRequest,
    ) -> impl Future<Output = Result<AgentRunResult, AppError>> + Send;

    /// Test and third-party runtimes that only support ephemeral sessions keep
    /// working for fresh queries; implementations that support continuation
    /// override this method.
    fn run_query_session(
        &self,
        request: AgentRunRequest,
        session_id: Option<&str>,
    ) -> impl Future<Output = Result<AgentRunResult, AppError>> + Send {
        let reusing_session = session_id.is_some();
        async move {
            if reusing_session {
                return Err(AppError::Runtime(
                    "this runtime does not support query session reuse".into(),
                ));
            }
            self.run_new_session(request).await
        }
    }
}

#[derive(Clone)]
pub struct OpenCodeAgent {
    config: Arc<Config>,
}

impl OpenCodeAgent {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    fn client(&self, workdir: &Path) -> Result<Client, AppError> {
        Ok(ClientBuilder::new()
            .base_url(&self.config.opencode_url)
            .directory(workdir.to_string_lossy())
            .timeout_secs(self.config.opencode_timeout_secs)
            .build()?)
    }
}

impl OpenCodeRuntime for OpenCodeAgent {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        let client = self.client(&request.workdir)?;

        let session = client
            .sessions()
            .create(&CreateSessionRequest {
                title: Some(request.title.clone()),
                permission: Some(all_permissions(&self.config.hidden_mcp)),
                ..Default::default()
            })
            .await?;

        run_session(
            client,
            session.id,
            request,
            self.config.clone(),
            SessionCleanup::Never,
        )
        .await
    }

    async fn run_query_session(
        &self,
        request: AgentRunRequest,
        session_id: Option<&str>,
    ) -> Result<AgentRunResult, AppError> {
        let client = self.client(&request.workdir)?;
        if let Some(session_id) = session_id {
            return run_session(
                client,
                session_id.to_string(),
                request,
                self.config.clone(),
                SessionCleanup::Never,
            )
            .await;
        }

        let session = client
            .sessions()
            .create(&CreateSessionRequest {
                title: Some(request.title.clone()),
                permission: Some(all_permissions(&self.config.hidden_mcp)),
                ..Default::default()
            })
            .await?;

        // A newly created query session is retained only after its answer is
        // delivered. If its first turn fails or the HTTP/MCP caller goes away,
        // deleting it avoids an unreachable session accumulating server-side.
        run_session(
            client,
            session.id,
            request,
            self.config.clone(),
            SessionCleanup::OnFailureOrCancellation,
        )
        .await
    }
}

enum SessionCleanup {
    /// A first query turn must make its session discoverable to retain it.
    OnFailureOrCancellation,
    /// A completed session remains in OpenCode's project history.
    Never,
}

/// Drive one OpenCode turn on a detached task. Detaching makes cleanup safe
/// when the request future is cancelled by a disconnected HTTP/MCP caller.
async fn run_session(
    client: Client,
    session_id: String,
    request: AgentRunRequest,
    config: Arc<Config>,
    cleanup: SessionCleanup,
) -> Result<AgentRunResult, AppError> {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = drive_session(&client, &session_id, request, &config).await;
        let delete_after_result =
            matches!(cleanup, SessionCleanup::OnFailureOrCancellation) && result.is_err();
        let receiver_dropped = result_tx.send(result).is_err();
        let delete = match cleanup {
            SessionCleanup::OnFailureOrCancellation => delete_after_result || receiver_dropped,
            SessionCleanup::Never => false,
        };
        if delete && let Err(error) = client.sessions().delete(&session_id).await {
            tracing::warn!(%session_id, %error, "failed to delete OpenCode session");
        }
    });
    result_rx
        .await
        .map_err(|_| AppError::Runtime("OpenCode session task aborted".into()))?
}

async fn drive_session(
    client: &Client,
    session_id: &str,
    request: AgentRunRequest,
    config: &Config,
) -> Result<AgentRunResult, AppError> {
    let mut subscription = client.subscribe_session(session_id)?;

    // Server-side live transcript (opt-in via the --transcript flag);
    // never affects the result returned to callers.
    let mut transcript = Transcript::new(config.transcript, session_id, &request.title);

    let prompt = PromptRequest {
        parts: vec![PromptPart::Text {
            text: request.prompt,
            synthetic: None,
            ignored: None,
            metadata: None,
        }],
        message_id: None,
        model: Some(parse_model(&config.opencode_model)),
        agent: None,
        no_reply: None,
        system: None,
        variant: None,
    };
    // One deadline for the whole turn: firing the prompt and collecting
    // the streamed answer are both agent work, so both count against the
    // configured session timeout. (A stalled `prompt_async` used to be
    // bounded only by the HTTP client timeout, doubling the worst case.)
    let deadline = Instant::now() + Duration::from_secs(config.opencode_timeout_secs);
    let turn = async {
        // Fire-and-forget: the synchronous `prompt` endpoint blocks until
        // the whole turn completes, so nothing on the already-open
        // subscription would be consumed (or rendered by the live
        // transcript) until the very end. `prompt_async` returns
        // immediately and the turn streams over the session subscription.
        client.messages().prompt_async(session_id, &prompt).await?;
        collect_events(&mut subscription, &mut transcript).await
    };

    let collected = match timeout_at(deadline, turn).await {
        Ok(collected) => collected?,
        Err(_) => return Err(AppError::Runtime("OpenCode session timed out".into())),
    };

    Ok(AgentRunResult {
        session_id: session_id.to_string(),
        answer: collected.answer,
        tool_events: collected.tool_events,
    })
}

#[derive(Default)]
struct CollectedEvents {
    /// Text of the latest assistant message only: earlier messages are
    /// intermediate steps (planning, tool commentary), not the answer.
    answer: String,
    streamed_text: bool,
    streamed_message: Option<String>,
    /// Latest text snapshot per part of the fallback message: OpenCode
    /// resends a part's whole text on every `message.part.updated`, so a new
    /// snapshot of the same part replaces the old one instead of appending.
    /// Keyed by the part identity (see [`part_identity`]).
    fallback_parts: Vec<(String, String)>,
    tool_events: Vec<serde_json::Value>,
}

impl CollectedEvents {
    /// Accumulate one streamed text delta. A new assistant message starts a
    /// new step, so its first delta discards the previous message's
    /// intermediate text — both the delta-accumulated answer and the part
    /// snapshots, which up to that point belonged to the earlier message
    /// (the server's `message.part.updated` events carry no message id, so
    /// snapshots cannot be attributed to a message on their own).
    fn apply_delta(&mut self, properties: &MessagePartEventProps) {
        if !is_text_delta(properties) {
            return;
        }
        let Some(delta) = properties.delta.as_deref() else {
            return;
        };
        if let Some(message_id) = &properties.message_id
            && self.streamed_message.as_deref() != Some(message_id)
        {
            self.answer.clear();
            self.fallback_parts.clear();
            self.streamed_message = Some(message_id.clone());
        }
        self.answer.push_str(delta);
        self.streamed_text = true;
    }

    /// Accumulate one part snapshot into the fallback used when no deltas
    /// were streamed at all.
    fn apply_part_update(&mut self, properties: &MessagePartEventProps) {
        let Some(Part::Text { text, .. }) = properties.part.as_ref() else {
            return;
        };
        let key = part_identity(properties);
        match self
            .fallback_parts
            .iter_mut()
            .find(|(part, _)| *part == key)
        {
            Some((_, snapshot)) => *snapshot = text.clone(),
            None => self.fallback_parts.push((key, text.clone())),
        }
    }
}

/// The identity a part snapshot is deduped under. The server's
/// `message.part.updated` event carries neither a message id nor a part
/// index (only `{sessionID, part, time}`); the stable identity is the
/// `prt…` id inside the part object, repeated on every snapshot resend.
/// Emitters that omit the part id (older servers, test fixtures) fall back
/// to the event's index; with neither, all snapshots share one slot.
fn part_identity(properties: &MessagePartEventProps) -> String {
    if let Some(Part::Text { id: Some(id), .. }) = properties.part.as_ref() {
        return id.clone();
    }
    format!("index:{:?}", properties.index)
}

impl CollectedEvents {
    fn push_tool_event(&mut self, event: &Event) {
        if let Ok(value) = serde_json::to_value(event) {
            self.tool_events.push(value);
        }
    }

    fn finish(mut self) -> Self {
        if !self.streamed_text && !self.fallback_parts.is_empty() {
            self.answer = self
                .fallback_parts
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
        }
        if let Some(marked) = extract_marked_answer(&self.answer) {
            self.answer = marked;
        }
        self
    }
}

/// The query prompt asks the model to wrap its final answer in these
/// markers. When a complete pair is present, its content is the answer
/// (marker-free even if the same message also contains narration); otherwise
/// callers fall back to the raw last-message text, so models that skip the
/// markers degrade gracefully instead of failing.
pub(crate) const ANSWER_OPEN: &str = "<noema-answer>";
pub(crate) const ANSWER_CLOSE: &str = "</noema-answer>";

fn extract_marked_answer(text: &str) -> Option<String> {
    // Scan closing markers right to left. The answer JSON itself may quote a
    // `</noema-answer>` marker, so the rightmost close is not necessarily the
    // protocol's: prefer the first span (from its matching open marker) that
    // parses as JSON, keeping the rightmost complete pair as the fallback
    // for non-JSON answers.
    let mut search = text;
    let mut fallback = None;
    while let Some(close) = search.rfind(ANSWER_CLOSE) {
        if let Some(open) = search[..close].rfind(ANSWER_OPEN) {
            let inner = search[open + ANSWER_OPEN.len()..close].trim();
            if !inner.is_empty() {
                if fallback.is_none() {
                    fallback = Some(inner.to_string());
                }
                if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
                    return Some(inner.to_string());
                }
            }
        }
        search = &search[..close];
    }
    fallback
}

/// Which streamed field a part delta carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeltaKind {
    /// Visible assistant text.
    Text,
    /// Reasoning / "thinking" content.
    Thinking,
}

/// Classify a `message.part.delta` event. The SDK models the streamed field
/// either through the updated `part` variant or, for bare deltas, through the
/// flattened `field` property ("text" / "reasoning"); absent both, the delta
/// is treated as visible text.
pub(crate) fn delta_kind(properties: &MessagePartEventProps) -> Option<DeltaKind> {
    match properties.part.as_ref() {
        Some(Part::Text { .. }) => Some(DeltaKind::Text),
        Some(Part::Reasoning { .. }) => Some(DeltaKind::Thinking),
        Some(_) => None,
        None => match properties
            .extra
            .get("field")
            .and_then(serde_json::Value::as_str)
        {
            Some("reasoning") | Some("thinking") => Some(DeltaKind::Thinking),
            _ => Some(DeltaKind::Text),
        },
    }
}

/// Whether a delta belongs to the visible answer. Only these deltas feed the
/// answer returned over HTTP and MCP; reasoning deltas are transcript-only.
pub(crate) fn is_text_delta(properties: &MessagePartEventProps) -> bool {
    matches!(delta_kind(properties), Some(DeltaKind::Text))
}

async fn collect_events(
    subscription: &mut opencode_rs::sse::SseSubscription<Event>,
    transcript: &mut Transcript,
) -> Result<CollectedEvents, AppError> {
    let mut collected = CollectedEvents::default();
    while let Some(event) = subscription.recv().await {
        transcript.event(&event);
        match &event {
            Event::SessionIdle { .. } => break,
            Event::SessionError { properties } => {
                return Err(AppError::Runtime(format!(
                    "OpenCode session error: {:?}",
                    properties.error
                )));
            }
            // Only visible text deltas feed the answer returned over
            // HTTP/MCP; reasoning deltas stay transcript-only.
            Event::MessagePartDelta { properties } => collected.apply_delta(properties),
            Event::MessagePartUpdated { properties } => collected.apply_part_update(properties),
            Event::MessageUpdated { properties } => {
                if properties.info.role == "assistant" {
                    collected.push_tool_event(&event);
                }
            }
            Event::CommandExecuted { .. }
            | Event::FileEdited { .. }
            | Event::McpToolsChanged { .. }
            | Event::PermissionAsked { .. } => collected.push_tool_event(&event),
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

fn all_permissions(hidden_mcp: &[String]) -> Ruleset {
    // Keep every OpenCode capability available, except the interactive
    // question tool: a headless knowledge service has no user-answer loop.
    // OpenCode evaluates the last matching rule, so the specific deny follows
    // the wildcard allow.
    let mut rules = vec![
        permission("*", "*", PermissionAction::Allow),
        permission("question", "*", PermissionAction::Deny),
    ];
    // Foreign MCP servers registered on the shared OpenCode Server for other
    // callers (e.g. the host task runtime). OpenCode wildcard-matches the
    // tool name (`<server>_<tool>`) and a pattern-`*` deny hides the tool
    // from the model entirely, so Noema sessions never call a task runtime
    // with a Noema job id — or re-enter the knowledge API recursively.
    for server in hidden_mcp {
        rules.push(permission(&format!("{server}_*"), "*", PermissionAction::Deny));
    }
    rules
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
        let rules = all_permissions(&[]);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], permission("*", "*", PermissionAction::Allow));
        assert_eq!(
            rules[1],
            permission("question", "*", PermissionAction::Deny)
        );
    }

    /// Foreign MCP servers are denied after the wildcard allow, so OpenCode's
    /// last-match evaluation hides their tools from Noema sessions while the
    /// rest of the toolset stays available.
    #[test]
    fn permissions_hide_foreign_mcp_servers() {
        let rules = all_permissions(&[
            "lexifact-agent-runtime".to_string(),
            "noema-knowledge".to_string(),
        ]);
        assert_eq!(rules.len(), 4);
        assert_eq!(
            rules[2],
            permission("lexifact-agent-runtime_*", "*", PermissionAction::Deny)
        );
        assert_eq!(
            rules[3],
            permission("noema-knowledge_*", "*", PermissionAction::Deny)
        );
    }

    #[test]
    fn marked_answer_extraction_takes_the_last_complete_pair() {
        let text = "我先查一下。<noema-answer>草稿</noema-answer>补充思考。\
                    <noema-answer>\n最终答案，引用 raw/a.md:3。\n</noema-answer>收尾旁白";
        assert_eq!(
            extract_marked_answer(text).as_deref(),
            Some("最终答案，引用 raw/a.md:3。")
        );
    }

    #[test]
    fn marked_answer_extraction_requires_a_complete_pair() {
        assert_eq!(extract_marked_answer("只有旁白，没有标记"), None);
        assert_eq!(extract_marked_answer("<noema-answer>只有开标签"), None);
        assert_eq!(
            extract_marked_answer("<noema-answer>  </noema-answer>"),
            None
        );
    }

    #[test]
    fn marked_answer_extraction_recovers_json_quoting_a_close_marker() {
        let text = r#"<noema-answer>{"answer":"see </noema-answer> for details","references":[]}</noema-answer>"#;
        let extracted = extract_marked_answer(text).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&extracted).is_ok());
        assert!(extracted.contains("for details"));
    }

    fn props(field: Option<&str>, part: Option<Part>) -> MessagePartEventProps {
        MessagePartEventProps {
            session_id: None,
            message_id: None,
            index: None,
            part,
            delta: None,
            extra: field
                .map(|value| serde_json::json!({ "field": value }))
                .unwrap_or(serde_json::json!({})),
        }
    }

    fn part(json: serde_json::Value) -> Part {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn delta_kind_prefers_the_part_variant_then_the_field() {
        let text = part(serde_json::json!({ "type": "text", "text": "hi" }));
        let reasoning = part(serde_json::json!({ "type": "reasoning", "text": "hm" }));
        assert_eq!(delta_kind(&props(None, Some(text))), Some(DeltaKind::Text));
        assert_eq!(
            delta_kind(&props(None, Some(reasoning))),
            Some(DeltaKind::Thinking)
        );
        assert_eq!(
            delta_kind(&props(Some("reasoning"), None)),
            Some(DeltaKind::Thinking)
        );
        assert_eq!(
            delta_kind(&props(Some("text"), None)),
            Some(DeltaKind::Text)
        );
        assert_eq!(delta_kind(&props(None, None)), Some(DeltaKind::Text));
    }

    #[test]
    fn only_text_deltas_feed_the_answer() {
        assert!(is_text_delta(&props(Some("text"), None)));
        assert!(is_text_delta(&props(None, None)));
        assert!(!is_text_delta(&props(Some("reasoning"), None)));
    }

    /// Production shape: `message.part.updated` carries no message id and
    /// no index — the identity is the part id inside the part object.
    fn text_update(part_id: &str, text: &str) -> MessagePartEventProps {
        props(
            None,
            Some(part(
                serde_json::json!({ "type": "text", "id": part_id, "text": text }),
            )),
        )
    }

    #[test]
    fn fallback_keeps_only_the_latest_snapshot_per_part() {
        // OpenCode resends a part's whole text on every update; the fallback
        // answer must hold each part once, not once per snapshot.
        let mut collected = CollectedEvents::default();
        collected.apply_part_update(&text_update("prt-1", "hel"));
        collected.apply_part_update(&text_update("prt-1", "hello"));
        collected.apply_part_update(&text_update("prt-2", "world"));
        assert_eq!(collected.finish().answer, "hello\nworld");
    }

    #[test]
    fn fallback_without_part_ids_falls_back_to_the_event_index() {
        let mut properties = props(
            None,
            Some(part(serde_json::json!({ "type": "text", "text": "hi" }))),
        );
        properties.index = Some(7);
        assert_eq!(part_identity(&properties), "index:Some(7)");
        assert_eq!(part_identity(&text_update("prt-9", "x")), "prt-9");
    }

    #[test]
    fn a_new_message_delta_discards_stale_fallback_snapshots() {
        let mut collected = CollectedEvents::default();
        collected.apply_part_update(&text_update("prt-1", "中间步骤"));
        let mut delta = props(Some("text"), None);
        delta.message_id = Some("m2".into());
        delta.delta = Some("最终答案".into());
        collected.apply_delta(&delta);
        // The delta-path answer wins and the stale snapshot is gone.
        assert_eq!(collected.finish().answer, "最终答案");
    }
}
