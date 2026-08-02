//! Optional human-readable live transcript of OpenCode sessions for the
//! server log.
//!
//! Enabled per process with the server's `--transcript` flag (the
//! `NOEMA_TRANSCRIPT` environment variable is its fallback, like every other
//! setting). While enabled, the runtime streams assistant text, reasoning
//! ("thinking"), tool invocations and results (including the built-in
//! `skill` tool), per-step statistics and session errors to stderr as they
//! arrive on the event stream. Assistant text and reasoning are previewed
//! per part with a bounded budget plus an omission note — full answers
//! travel over HTTP/MCP, the log only needs to show what the agent is
//! doing. This is strictly server-side observability: the HTTP and MCP
//! surfaces keep returning only the final text answer.

use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anstyle::{AnsiColor, Style};
use opencode_rs::types::{
    event::{Event, MessagePartEventProps},
    message::{Part, ToolState},
};
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use crate::runtime::{DeltaKind, delta_kind};

const DIM: Style = Style::new().dimmed();
const BOLD: Style = Style::new().bold();
const RED: Style = AnsiColor::Red.on_default();
const GREEN: Style = AnsiColor::Green.on_default();
const YELLOW: Style = AnsiColor::Yellow.on_default();
const CYAN: Style = AnsiColor::Cyan.on_default();
const MAGENTA: Style = AnsiColor::Magenta.on_default();

/// Visible assistant text is previewed up to this many characters per part.
const TEXT_PREVIEW_CHARS: usize = 240;
/// Reasoning ("thinking") is noisier: a smaller preview keeps the log usable.
const THINKING_PREVIEW_CHARS: usize = 120;

/// The currently open streamed line, if any. Text and thinking deltas are
/// written without a trailing newline so they read as live output; anything
/// that emits a whole line must close the open line first. The owner id
/// keeps concurrent sessions apart, and the part key keeps consecutive
/// parts of the same kind on their own gutter line instead of gluing
/// together.
struct OpenLine {
    owner: usize,
    kind: DeltaKind,
    part: String,
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
    id: usize,
    announced: HashSet<String>,
    finished: HashSet<String>,
    /// Preview characters already streamed per text/reasoning part, keyed by
    /// message id and part index.
    previewed: HashMap<String, usize>,
    /// Parts whose "N more characters omitted" note has been printed.
    omission_noted: HashSet<String>,
    steps: u32,
    tokens_in: u64,
    tokens_out: u64,
    cost: f64,
}

impl Transcript {
    pub(crate) fn new(enabled: bool, session_id: &str, title: &str) -> Self {
        let transcript = Self {
            enabled,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            announced: HashSet::new(),
            finished: HashSet::new(),
            previewed: HashMap::new(),
            omission_noted: HashSet::new(),
            steps: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
        };
        let short = session_id
            .get(session_id.len().saturating_sub(8)..)
            .unwrap_or(session_id);
        transcript.line(&paint(BOLD, &format!("┌─ opencode · {title} · …{short}")));
        transcript
    }

    /// Render one event from the session subscription.
    pub(crate) fn event(&mut self, event: &Event) {
        if !self.enabled {
            return;
        }
        match event {
            Event::MessagePartDelta { properties } => self.delta(properties),
            Event::MessagePartUpdated { properties } => self.part_update(properties),
            Event::SessionError { properties } => self.line(&paint(
                RED,
                &format!("├ ✖ session error: {:?}", properties.error),
            )),
            Event::SessionIdle { .. } => {
                self.close_line();
                self.raw_line(&paint(
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

    /// Stream one text or thinking delta, capped by the per-part preview
    /// budget. Whitespace-only deltas are skipped so the gutter never fills
    /// with empty lines.
    fn delta(&mut self, properties: &MessagePartEventProps) {
        let Some(kind) = delta_kind(properties) else {
            return;
        };
        let Some(delta) = properties.delta.as_deref() else {
            return;
        };
        if delta.trim().is_empty() {
            return;
        }
        let key = part_key(properties);
        let budget = preview_chars(kind);
        let rendered = self.previewed.get(&key).copied().unwrap_or(0);
        if rendered >= budget {
            return;
        }
        let piece: String = delta.chars().take(budget - rendered).collect();
        // Keep the gutter on wrapped lines of multi-line chunks.
        let content = piece.replace('\n', "\n│  ");
        let mut open = lock_open_line();
        if open
            .as_ref()
            .map(|line| (line.owner, line.kind, line.part.as_str()))
            != Some((self.id, kind, key.as_str()))
        {
            if open.is_some() {
                let _ = writeln!(stderr());
            }
            let prefix = match kind {
                DeltaKind::Text => paint(CYAN, "├ ✍ "),
                DeltaKind::Thinking => paint(MAGENTA, "├ 💭 "),
            };
            let _ = write!(stderr(), "{prefix}");
            *open = Some(OpenLine {
                owner: self.id,
                kind,
                part: key.clone(),
            });
        }
        let styled = match kind {
            DeltaKind::Text => paint(BOLD, &content),
            DeltaKind::Thinking => paint(DIM, &content),
        };
        let _ = write!(stderr(), "{styled}");
        let _ = stderr().flush();
        *self.previewed.entry(key).or_insert(0) += piece.chars().count();
    }

    fn part_update(&mut self, properties: &MessagePartEventProps) {
        match properties.part.as_ref() {
            Some(Part::Text { text, .. }) => self.preview_omission(properties, text),
            Some(Part::Reasoning { text, .. }) => self.preview_omission(properties, text),
            Some(part) => self.part(part),
            None => {}
        }
    }

    /// Once a previewed part's full text is known, note how much of it was
    /// left out of the log.
    fn preview_omission(&mut self, properties: &MessagePartEventProps, full: &str) {
        let key = part_key(properties);
        let rendered = self.previewed.get(&key).copied().unwrap_or(0);
        if rendered == 0 || self.omission_noted.contains(&key) {
            return;
        }
        let total = full.chars().count();
        if total <= rendered {
            return;
        }
        self.omission_noted.insert(key);
        self.line(&paint(
            DIM,
            &format!("│  …（另有约 {} 字未显示）", total - rendered),
        ));
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
                self.line(&paint(
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
                self.line(&paint(YELLOW, &format!("├ ↻ retry #{attempt}")))
            }
            Part::Compaction { auto, .. } => self.line(&paint(
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
                paint(GREEN, &format!("├ 🎯 skill · {name}"))
            } else {
                let args = resolved.map(tool_args).unwrap_or_else(|| "…".to_string());
                let line = if args.is_empty() {
                    format!("├ 🔧 {tool}")
                } else {
                    format!("├ 🔧 {tool} {args}")
                };
                paint(CYAN, &line)
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
                let summary = tool_output_summary(&completed.output);
                let text = if summary.is_empty() {
                    "│  ↳ ok".to_string()
                } else {
                    format!("│  ↳ {summary}")
                };
                self.line(&paint(DIM, &text));
            }
            ToolState::Error(error) => {
                self.finished.insert(call_id.to_string());
                self.line(&paint(
                    RED,
                    &format!("│  ↳ error: {}", summarize(&error.error, 160)),
                ));
            }
            _ => {}
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
            let _ = writeln!(stderr());
            let _ = stderr().flush();
        }
    }

    fn raw_line(&self, text: &str) {
        let _ = writeln!(stderr(), "{text}");
        let _ = stderr().flush();
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
            let _ = writeln!(stderr());
            let _ = stderr().flush();
        }
    }
}

/// Argument keys worth displaying in full (paths, patterns, shell commands),
/// in the order the most relevant one is surfaced first.
const FULL_VALUE_KEYS: [&str; 8] = [
    "filePath",
    "command",
    "pattern",
    "path",
    "directory",
    "workdir",
    "cwd",
    "notebookPath",
];

/// Tool arguments for the log: the first path-like argument comes first and
/// in full; everything else stays compact so `content` blobs never flood the
/// line. No overall length cap — full paths are the point.
fn tool_args(input: &serde_json::Value) -> String {
    let serde_json::Value::Object(map) = input else {
        return summarize(&input.to_string(), 100);
    };
    if map.is_empty() {
        return String::new();
    }
    let primary = FULL_VALUE_KEYS
        .iter()
        .find_map(|key| map.get(*key).map(|value| (*key, value)));
    let mut pieces = Vec::new();
    if let Some((_, value)) = primary {
        pieces.push(full_scalar(value));
    }
    for (key, value) in map {
        if primary.is_some_and(|(primary_key, _)| key == primary_key) {
            continue;
        }
        let rendered = if FULL_VALUE_KEYS.contains(&key.as_str()) {
            full_scalar(value)
        } else {
            compact_scalar(value)
        };
        pieces.push(format!("{key}={rendered}"));
    }
    pieces.join(" ")
}

/// A scalar argument shown in full (paths, commands); newlines become
/// spaces to keep the call on one log line.
fn full_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.replace(['\n', '\r'], " "),
        other => other.to_string(),
    }
}

/// Compact rendering of a tool result. OpenCode's file tools report
/// `<path>…</path> <type>…</type> …`; surface the full path (plus the entry
/// count for directories) instead of raw XML-ish text.
fn tool_output_summary(output: &str) -> String {
    if let Some(path) = tag_value(output, "<path>", "</path>") {
        let kind = tag_value(output, "<type>", "</type>").unwrap_or_default();
        return match kind {
            "directory" => {
                let entries = tag_value(output, "<entries>", "</entries>")
                    .map(|list| list.split_whitespace().count())
                    .unwrap_or(0);
                format!("目录 {path}（{entries} 项）")
            }
            "file" => format!("文件 {path}"),
            _ => path.to_string(),
        };
    }
    summarize(output, 120)
}

fn tag_value<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = start + text[start..].find(close)?;
    Some(&text[start..end])
}

/// Stable identity of a streamed part: message id plus part index.
fn part_key(properties: &MessagePartEventProps) -> String {
    format!("{:?}#{:?}", properties.message_id, properties.index)
}

fn preview_chars(kind: DeltaKind) -> usize {
    match kind {
        DeltaKind::Text => TEXT_PREVIEW_CHARS,
        DeltaKind::Thinking => THINKING_PREVIEW_CHARS,
    }
}

/// Embed one style's escape codes around `text`. The stderr writer strips
/// the codes when colors are not appropriate (see [`stderr`]).
fn paint(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

/// The transcript writer: anstream honours NO_COLOR / FORCE_COLOR /
/// CLICOLOR and strips embedded styles when stderr is not a terminal.
fn stderr() -> anstream::AutoStream<std::io::Stderr> {
    anstream::AutoStream::auto(std::io::stderr())
}

/// Collapse whitespace and truncate to a display-width budget (terminal
/// columns, so CJK counts double and codepoints are never split).
fn summarize(text: &str, max_width: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.width() <= max_width {
        return collapsed;
    }
    let (head, _) = collapsed.unicode_truncate(max_width.saturating_sub(1));
    format!("{head}…")
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

    #[test]
    fn summarize_collapses_whitespace_and_truncates_by_width() {
        assert_eq!(summarize("a\n\n b\t c", 10), "a b c");
        // CJK characters are two columns wide: budget 4 fits one plus the
        // ellipsis.
        assert_eq!(summarize("会话上下文与会话执行", 4), "会…");
    }

    #[test]
    fn tool_args_surfaces_full_paths_first_and_compacts_blobs() {
        let long_path = format!("/root/very/long/library/root/wiki/{}.md", "n".repeat(80));
        let args = json!({ "content": "x".repeat(300), "filePath": long_path, "limit": 10 });
        let rendered = tool_args(&args);
        assert!(rendered.starts_with(&long_path));
        assert!(rendered.contains("limit=10"));
        assert!(rendered.contains("content="));
        assert!(!rendered.contains(&"x".repeat(300)));
    }

    #[test]
    fn tool_output_summary_prefers_the_full_path() {
        let read_output = "<path>/root/data/libraries/lib/wiki/node.md</path> <type>file</type> <content> 1: ---…";
        assert_eq!(
            tool_output_summary(read_output),
            "文件 /root/data/libraries/lib/wiki/node.md"
        );
        let dir_output = "<path>/root/data/libraries/lib/raw</path> <type>directory</type> <entries> a.md b.md c.txt </entries>";
        assert_eq!(
            tool_output_summary(dir_output),
            "目录 /root/data/libraries/lib/raw（3 项）"
        );
        assert_eq!(
            tool_output_summary("Wrote file successfully."),
            "Wrote file successfully."
        );
    }

    #[test]
    fn skill_name_reads_the_skill_tool_input() {
        assert_eq!(
            skill_name(&json!({ "name": "knowledge-compiler" })).as_deref(),
            Some("knowledge-compiler")
        );
        assert_eq!(skill_name(&json!({})), None);
    }
}
