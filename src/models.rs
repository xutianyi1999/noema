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
    /// Re-derive the wiki nodes and graphify graph after a document was
    /// deleted (the mirror image of an ingest compiling an addition).
    Maintain,
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

/// The string conversions a lowercase string enum needs at the SQLite
/// boundary and in display/parse paths, all keyed off one variant-to-string
/// table so the five impls cannot drift apart.
macro_rules! string_enum {
    ($name:ident, $label:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!("unknown {}: {other}", $label)),
                }
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::Borrowed(ValueRef::Text(
                    self.as_str().as_bytes(),
                )))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value
                    .as_str()?
                    .parse()
                    .map_err(|_| FromSqlError::InvalidType)
            }
        }
    };
}

string_enum!(JobKind, "job kind", {
    Ingest => "ingest",
    Maintain => "maintain"
});
string_enum!(JobState, "job state", {
    Queued => "queued",
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    Skipped => "skipped",
});

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
pub struct SubmitDocumentsRequest {
    pub documents: Vec<DocumentInput>,
}

/// One batch entry's outcome. Every entry is stored; only the skipped ones
/// stay out of the batch's ingestion job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmittedDocument {
    /// The filename as submitted (NFC-normalized); a duplicate entry's
    /// stored record may carry the original file's name instead.
    pub filename: String,
    /// Library-relative path under `raw/`; two entries with identical
    /// content share one path.
    pub document_path: String,
    /// `true` when sha256 dedupe matched content already in the library.
    pub duplicate: bool,
    /// `true` only for the genuine no-op: duplicate content already
    /// compiled into wiki nodes. A duplicate a failed job left uncompiled
    /// reports `false` — it runs in the batch's ingestion.
    pub skipped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitDocumentsResponse {
    pub library_id: String,
    /// The one ingestion job covering every non-skipped entry; entries all
    /// compile together in a single staging workspace and session.
    pub job_id: String,
    pub documents: Vec<SubmittedDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub prompt: String,
    /// Continue a prior successful query in this library. When omitted, the
    /// service creates a new OpenCode session.
    pub session_id: Option<String>,
}

/// Outcome of a document deletion. The document row and its `raw/` file are
/// removed synchronously; the returned maintenance job re-derives the wiki
/// nodes and the graphify graph via an OpenCode session (poll it via the job
/// status endpoints).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteDocumentResponse {
    pub library_id: String,
    pub job_id: String,
    pub filename: String,
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
    /// Continue a prior successful query in this library. When omitted, the
    /// service creates a new OpenCode session.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpIngestRequest {
    pub library_id: String,
    pub documents: Vec<DocumentInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpEnsureLibraryRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpJobRequest {
    pub library_id: String,
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpListDocumentsRequest {
    pub library_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpDeleteDocumentRequest {
    pub library_id: String,
    pub filename: String,
}

/// One verified citation attached to a query response. The Agent declares
/// only `source` + `quote` (plus optional offsets); every other field is
/// computed and verified server-side against the library's files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    /// Stable 1-based citation id matching the `[n]` markers in the answer;
    /// ids of citations that failed verification are skipped, never reused.
    pub id: u32,
    /// Display name: the document's registered title (filename stem as
    /// fallback).
    pub title: String,
    /// Library-relative path under `raw/`. Citations stand on primary
    /// evidence only, so wiki nodes are never cited.
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
