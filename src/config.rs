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
}
