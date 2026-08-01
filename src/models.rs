use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub prompt: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub title: String,
    pub source: String,
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
    pub kind: String,
    pub status: String,
    pub error: Option<String>,
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub data_dir: String,
    pub opencode_url: String,
    pub configured_model: String,
}
