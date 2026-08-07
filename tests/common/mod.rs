//! Shared fixtures for the integration test binaries.

use std::path::PathBuf;

use noema::Config;

/// Test configuration with a short agent timeout. The runtime is injected
/// per test binary, so no test ever reaches a real OpenCode server.
pub fn config(data_dir: PathBuf, auth_token: Option<&str>) -> Config {
    Config {
        data_dir,
        opencode_url: "http://127.0.0.1:4096".into(),
        opencode_model: "opencode/deepseek-v4-flash-free".into(),
        opencode_timeout_secs: 5,
        transcript: false,
        max_sessions: 4,
        auth_token: auth_token.map(str::to_string),
        hidden_mcp: vec![],
        hidden_skills: vec![],
    }
}
