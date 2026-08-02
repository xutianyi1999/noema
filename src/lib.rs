//! Noema: an OpenCode-driven, isolated text knowledge-base service.

pub mod config;
pub mod error;
pub mod http;
pub mod mcp;
pub mod models;
pub mod runtime;
pub mod service;
pub mod snapshot;
pub mod storage;
pub(crate) mod transcript;

pub use config::Config;
pub use error::AppError;
pub use service::AppService;
