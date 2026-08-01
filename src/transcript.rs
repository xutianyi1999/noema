//! Optional human-readable live transcript of OpenCode sessions for the
//! server log.
//!
//! Enabled per process with `NOEMA_TRANSCRIPT=1`. While enabled, the runtime
//! streams assistant text, reasoning ("thinking"), tool invocations and
//! results (including the built-in `skill` tool), per-step statistics and
//! session errors to stderr as they arrive on the event stream. This is
//! strictly server-side observability: the HTTP and MCP surfaces keep
//! returning only the final text answer.

use std::{
    collections::HashSet,
    env,
    io::{IsTerminal, Write as _},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use opencode_rs::types::{
    event::{Event, MessagePartEventProps},
    message::{Part, ToolState},
};

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const RESET: &str = "\x1b[0m";

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

/// Whether a delta belongs to the visible answer. The runtime appends only
/// these to the answer returned over HTTP and MCP; reasoning deltas are
/// transcript-only.
pub(crate) fn is_text_delta(properties: &MessagePartEventProps) -> bool {
    matches!(delta_kind(properties), Some(DeltaKind::Text))
}

/// The currently open streamed line, if any. Text and thinking deltas are
/// written without a trailing newline so they read as live output; anything
/// that emits a whole line must close the open line first. The owner id keeps
/// concurrent sessions from appending into each other's line: a mismatch
/// closes and re-opens the gutter instead.
struct OpenLine {
    owner: usize,
    kind: DeltaKind,
}

static OPEN_LINE: Mutex<Option<OpenLine>> = Mutex::new(None);
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

fn lock_open_line() -> std::sync::MutexGuard<'static, Option<OpenLine>> {
    OPEN_LINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Per-session transcript renderer.
pub(crate) struct Transcript {
    enabled: bool,
    color: bool,
    id: usize,
    announced: HashSet<String>,
    finished: HashSet<String>,
    steps: u32,
    tokens_in: u64,
    tokens_out: u64,
    cost: f64,
}

impl Transcript {
    pub(crate) fn new(session_id: &str, title: &str) -> Self {
        let enabled = env::var("NOEMA_TRANSCRIPT")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        let color = enabled && std::io::stderr().is_terminal() && env::var_os("NO_COLOR").is_none();
        let transcript = Self {
            enabled,
            color,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            announced: HashSet::new(),
            finished: HashSet::new(),
            steps: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
        };
        let short = session_id
            .get(session_id.len().saturating_sub(8)..)
            .unwrap_or(session_id);
        transcript.line(&transcript.paint(BOLD, &format!("┌─ opencode · {title} · …{short}")));
        transcript
    }

    /// Render one event from the session subscription.
    pub(crate) fn event(&mut self, event: &Event) {
        if !self.enabled {
            return;
        }
        match event {
            Event::MessagePartDelta { properties } => self.delta(properties),
            Event::MessagePartUpdated { properties } => {
                if let Some(part) = properties.part.as_ref() {
                    self.part(part);
                }
            }
            Event::SessionError { properties } => {
                self.line(&self.paint(RED, &format!("├ ✖ session error: {:?}", properties.error)))
            }
            Event::SessionIdle { .. } => {
                self.close_line();
                self.raw_line(&self.paint(
                    DIM,
                    &format!(
                        "└─ idle · {} steps · {} in / {} out tokens · ${:.4}",
                        self.steps, self.tokens_in, self.tokens_out, self.cost
                    ),
                ));
            }
            _ => {}
        }
    }

    /// Stream one text or thinking delta.
    fn delta(&self, properties: &MessagePartEventProps) {
        let Some(kind) = delta_kind(properties) else {
            return;
        };
        let Some(delta) = properties.delta.as_deref() else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        // Keep the gutter on wrapped lines of multi-line chunks.
        let content = delta.replace('\n', "\n│  ");
        let mut open = lock_open_line();
        if open.as_ref().map(|line| (line.owner, line.kind)) != Some((self.id, kind)) {
            if open.is_some() {
                let _ = writeln!(std::io::stderr());
            }
            let prefix = match kind {
                DeltaKind::Text => self.paint(CYAN, "├ ✍ "),
                DeltaKind::Thinking => self.paint(MAGENTA, "├ 💭 "),
            };
            let _ = write!(std::io::stderr(), "{prefix}");
            *open = Some(OpenLine {
                owner: self.id,
                kind,
            });
        }
        let styled = match kind {
            DeltaKind::Text => self.paint(BOLD, &content),
            DeltaKind::Thinking => self.paint(DIM, &content),
        };
        let _ = write!(std::io::stderr(), "{styled}");
        let _ = std::io::stderr().flush();
    }

    fn part(&mut self, part: &Part) {
        match part {
            Part::Tool {
                call_id,
                tool,
                input,
                state,
                ..
            } => self.tool(call_id, tool, input, state.as_ref()),
            Part::StepFinish {
                reason,
                tokens,
                cost,
                ..
            } => {
                self.steps += 1;
                self.cost += cost;
                if let Some(tokens) = tokens {
                    self.tokens_in += tokens.input;
                    self.tokens_out += tokens.output;
                }
                self.line(&self.paint(
                    DIM,
                    &format!(
                        "├ · step {} · {reason} · {} in / {} out tokens",
                        self.steps, self.tokens_in, self.tokens_out
                    ),
                ));
            }
            Part::Subtask {
                description, agent, ..
            } => self.line(&format!(
                "├ ▸ subtask → {agent}: {}",
                summarize(description, 80)
            )),
            Part::Retry { attempt, .. } => {
                self.line(&self.paint(YELLOW, &format!("├ ↻ retry #{attempt}")))
            }
            Part::Compaction { auto, .. } => self.line(&self.paint(
                YELLOW,
                &format!(
                    "├ ≡ context compaction{}",
                    if *auto { " (auto)" } else { "" }
                ),
            )),
            _ => {}
        }
    }

    fn tool(
        &mut self,
        call_id: &str,
        tool: &str,
        input: &serde_json::Value,
        state: Option<&ToolState>,
    ) {
        // OpenCode first streams tool parts with a null part-level `input`
        // and fills the arguments in through state updates; resolve from
        // either and defer the announcement until arguments (or a terminal
        // state) are known, so calls never render as `tool null`.
        let resolved = tool_input(input, state);
        let terminal = matches!(state, Some(ToolState::Completed(_) | ToolState::Error(_)));
        if !self.announced.contains(call_id) && (resolved.is_some() || terminal) {
            self.announced.insert(call_id.to_string());
            // OpenCode loads skills through the built-in `skill` tool; give
            // those their own glyph and surface the skill name.
            let line = if tool == "skill" {
                let name = resolved.and_then(skill_name).unwrap_or_else(|| "?".into());
                self.paint(GREEN, &format!("├ 🎯 skill · {name}"))
            } else {
                let args = resolved
                    .map(compact_args)
                    .unwrap_or_else(|| "…".to_string());
                self.paint(CYAN, &format!("├ 🔧 {tool} {args}"))
            };
            self.line(&line);
        }
        let Some(state) = state else {
            return;
        };
        if self.finished.contains(call_id) {
            return;
        }
        match state {
            ToolState::Completed(completed) => {
                self.finished.insert(call_id.to_string());
                let summary = summarize(&completed.output, 120);
                let text = if summary.is_empty() {
                    "│  ↳ ok".to_string()
                } else {
                    format!("│  ↳ {summary}")
                };
                self.line(&self.paint(DIM, &text));
            }
            ToolState::Error(error) => {
                self.finished.insert(call_id.to_string());
                self.line(&self.paint(
                    RED,
                    &format!("│  ↳ error: {}", summarize(&error.error, 160)),
                ));
            }
            _ => {}
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    /// Emit one self-contained line, closing any open streamed line first.
    fn line(&self, text: &str) {
        if !self.enabled {
            return;
        }
        self.close_line();
        self.raw_line(text);
    }

    fn close_line(&self) {
        let mut open = lock_open_line();
        if open.take().is_some() {
            let _ = writeln!(std::io::stderr());
            let _ = std::io::stderr().flush();
        }
    }

    fn raw_line(&self, text: &str) {
        let _ = writeln!(std::io::stderr(), "{text}");
        let _ = std::io::stderr().flush();
    }
}

impl Drop for Transcript {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        // Error and timeout paths never reach SessionIdle; never leave this
        // session's streamed line dangling into the next output.
        let mut open = lock_open_line();
        if open.as_ref().is_some_and(|line| line.owner == self.id) {
            open.take();
            let _ = writeln!(std::io::stderr());
            let _ = std::io::stderr().flush();
        }
    }
}

/// Collapse whitespace and truncate to a display budget (in characters, so
/// CJK text is not split mid-codepoint).
fn summarize(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Tool arguments, preferring the part-level `input` and falling back to the
/// latest state update, where OpenCode fills them in. Empty objects count as
/// not yet known: early states carry `{}` until completion fills the real
/// arguments.
fn tool_input<'a>(
    input: &'a serde_json::Value,
    state: Option<&'a ToolState>,
) -> Option<&'a serde_json::Value> {
    if has_args(input) {
        return Some(input);
    }
    match state? {
        ToolState::Pending(state) if has_args(&state.input) => Some(&state.input),
        ToolState::Running(state) if has_args(&state.input) => Some(&state.input),
        ToolState::Completed(state) if has_args(&state.input) => Some(&state.input),
        _ => None,
    }
}

fn has_args(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Object(map) => !map.is_empty(),
        _ => true,
    }
}

/// Render tool input as compact `key=value` pairs on one line.
fn compact_args(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) if !map.is_empty() => {
            let pairs = map
                .iter()
                .map(|(key, value)| format!("{key}={}", compact_scalar(value)))
                .collect::<Vec<_>>()
                .join(" ");
            summarize(&pairs, 100)
        }
        _ => summarize(&input.to_string(), 100),
    }
}

fn compact_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => summarize(text, 60),
        other => summarize(&other.to_string(), 60),
    }
}

fn skill_name(input: &serde_json::Value) -> Option<String> {
    input
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(field: Option<&str>, part: Option<Part>) -> MessagePartEventProps {
        MessagePartEventProps {
            session_id: None,
            message_id: None,
            index: None,
            part,
            delta: None,
            extra: field
                .map(|value| json!({ "field": value }))
                .unwrap_or(json!({})),
        }
    }

    fn part(json: serde_json::Value) -> Part {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn summarize_collapses_whitespace_and_truncates() {
        assert_eq!(summarize("a\n\n b\t c", 10), "a b c");
        assert_eq!(summarize("会话上下文与会话执行", 4), "会话上下…");
    }

    #[test]
    fn compact_args_renders_key_value_pairs() {
        let args = json!({ "filePath": "purpose.md", "limit": 10 });
        let rendered = compact_args(&args);
        assert!(rendered.contains("filePath=purpose.md"));
        assert!(rendered.contains("limit=10"));
    }

    #[test]
    fn skill_name_reads_the_skill_tool_input() {
        assert_eq!(
            skill_name(&json!({ "name": "knowledge-compiler" })).as_deref(),
            Some("knowledge-compiler")
        );
        assert_eq!(skill_name(&json!({})), None);
    }

    #[test]
    fn delta_kind_prefers_the_part_variant_then_the_field() {
        let text = part(json!({ "type": "text", "text": "hi" }));
        let reasoning = part(json!({ "type": "reasoning", "text": "hm" }));
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
}
