use std::{net::SocketAddr, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub opencode_url: String,
    pub opencode_model: String,
    pub opencode_timeout_secs: u64,
    pub transcript: bool,
    /// Global cap on concurrently running OpenCode sessions (ingests plus
    /// queries); further requests queue until a slot frees up.
    pub max_sessions: usize,
    /// Optional bearer token guarding the HTTP API. `None` keeps the API
    /// open, which is only appropriate when bound to loopback; `Some`
    /// requires `Authorization: Bearer <token>` on every route except the
    /// health probe, making remote/container-network deployments safe.
    pub auth_token: Option<String>,
}
