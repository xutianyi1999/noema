use std::{env, net::SocketAddr, path::PathBuf};

use crate::error::AppError;

pub const DEFAULT_MODEL: &str = "opencode/deepseek-v4-flash-free";

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub opencode_url: String,
    pub opencode_model: String,
    pub opencode_timeout_secs: u64,
    pub graphify_bin: String,
    pub install_graphify: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        let bind = env::var("NOEMA_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
            .parse()
            .map_err(|error| AppError::BadRequest(format!("invalid NOEMA_BIND: {error}")))?;

        Ok(Self {
            data_dir: PathBuf::from(env::var("NOEMA_DATA_DIR").unwrap_or_else(|_| "data".into())),
            bind,
            opencode_url: env::var("OPENCODE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:4096".into()),
            opencode_model: env::var("OPENCODE_TEST_MODEL")
                .or_else(|_| env::var("OPENCODE_MODEL"))
                .unwrap_or_else(|_| DEFAULT_MODEL.into()),
            opencode_timeout_secs: env::var("OPENCODE_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1800),
            graphify_bin: env::var("GRAPHIFY_BIN").unwrap_or_else(|_| "graphify".into()),
            install_graphify: env::var("NOEMA_INSTALL_GRAPHIFY")
                .map(|value| value != "0" && value != "false")
                .unwrap_or(true),
        })
    }
}
