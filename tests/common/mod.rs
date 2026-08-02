//! Shared fixtures for the integration test binaries.

use std::{net::SocketAddr, path::PathBuf};

use noema::Config;

/// Test configuration: loopback bind on a kernel-assigned port and a short
/// agent timeout. The runtime is injected per test binary, so no test ever
/// reaches a real OpenCode server.
pub fn config(data_dir: PathBuf, auth_token: Option<&str>) -> Config {
    Config {
        data_dir,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        opencode_url: "http://127.0.0.1:4096".into(),
        opencode_model: "opencode/deepseek-v4-flash-free".into(),
        opencode_timeout_secs: 5,
        transcript: false,
        max_sessions: 4,
        auth_token: auth_token.map(str::to_string),
    }
}
