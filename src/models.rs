use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The kind of an asynchronous content-library job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Ingest,
}

/// Lifecycle state of a job (also reused for query runs, which only ever
/// use `Running`, `Completed` and `Failed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl JobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
        }
    }
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for JobKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ingest" => Ok(Self::Ingest),
            other => Err(format!("unknown job kind: {other}")),
        }
    }
}

impl FromStr for JobState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            other => Err(format!("unknown job state: {other}")),
        }
    }
}

impl ToSql for JobKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.as_str().as_bytes(),
        )))
    }
}

impl ToSql for JobState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.as_str().as_bytes(),
        )))
    }
}

impl FromSql for JobKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|_| FromSqlError::InvalidType)
    }
}

impl FromSql for JobState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|_| FromSqlError::InvalidType)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateLibraryRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Library {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub root: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentInput {
    pub filename: String,
    pub content: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitDocumentResponse {
    pub library_id: String,
    pub job_id: String,
    pub document_path: Option<String>,
    /// `true` only for the genuine no-op: identical content already
    /// compiled into wiki nodes. A resubmission of content a failed job
    /// left uncompiled reports `false` — it runs a real ingestion.
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub prompt: String,
}

/// The JSON contract the query Agent must output inside its `<noema-answer>`
/// marker. The JSON Schema embedded in the query prompt is generated from
/// this type (`schemars`) and incoming answers are validated against the
/// same schema (`jsonschema`), so prompt and validator never drift apart.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentAnswer {
    /// The answer body, in whatever format the user's question asks for
    /// (Markdown or otherwise). Every factual claim ends with an `[n]`
    /// marker whose n is the 1-based index into `references`.
    pub answer: String,
    /// The sources the answer draws on.
    #[serde(default)]
    pub references: Vec<AgentReference>,
}

/// One source citation declared by the Agent. Only `source` and `quote` are
/// required; offsets are an optional accelerator the server verifies before
/// trusting and recomputes otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentReference {
    /// Library-relative path under `raw/` or `wiki/`.
    pub source: String,
    /// The cited passage, copied verbatim from the source file; verified
    /// server-side to be an exact substring.
    pub quote: String,
    /// The source's own address for the passage, in the source's own
    /// numbering (e.g. `第三十三条第二款`, `5.2.1`); a human-facing label
    /// the server passes through without interpreting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    /// Unicode character offset of the quote's first character (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
    /// Unicode character offset just past the quote's last character
    /// (optional, exclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpQueryRequest {
    pub library_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpIngestRequest {
    pub library_id: String,
    pub filename: String,
    pub content: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpJobRequest {
    pub library_id: String,
    pub job_id: String,
}

/// One verified citation attached to a query response. The Agent declares
/// only `source` + `quote` (plus optional offsets); every other field is
/// computed and verified server-side against the library's files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    /// Stable 1-based citation id matching the `[n]` markers in the answer;
    /// ids of citations that failed verification are skipped, never reused.
    pub id: u32,
    /// Display name: the document's registered title for `raw/` sources
    /// (filename stem as fallback), the node stem for `wiki/` sources.
    pub title: String,
    /// Library-relative path, under `raw/` or `wiki/`.
    pub source: String,
    /// The source's own address for the cited passage (e.g. `第三十三条`),
    /// exactly as the Agent declared it; not interpreted by the server.
    pub locator: Option<String>,
    /// The cited passage, verified to be a verbatim substring of the source;
    /// `None` only for fallback text-scanned citations.
    pub quote: Option<String>,
    /// RFC 5147 style Unicode character offsets into the source file
    /// (`end` exclusive); `None` only for fallback citations.
    pub start: Option<usize>,
    pub end: Option<usize>,
    /// 1-based inclusive line range covering the quote; `None` for fallback.
    pub lines: Option<(usize, usize)>,
    /// The compiled wiki node matching a `raw/` source, when one exists.
    pub node: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub query_id: String,
    pub library_id: String,
    pub session_id: String,
    pub answer: String,
    pub references: Vec<Reference>,
    pub tool_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub job_id: String,
    pub library_id: String,
    pub kind: JobKind,
    pub status: JobState,
    pub error: Option<String>,
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: &'static str,
    /// Server internals, reported only when the probe is authorized (or the
    /// API runs without authentication); absent from open probes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opencode_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_model: Option<String>,
}
