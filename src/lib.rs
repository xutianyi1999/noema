//! Noema: an OpenCode-driven, isolated text knowledge-base service.

pub(crate) mod answer;
pub(crate) mod bootstrap;
pub mod config;
pub mod error;
pub mod http;
pub(crate) mod mcp;
pub mod models;
pub(crate) mod references;
pub mod runtime;
pub mod service;
pub(crate) mod snapshot;
pub(crate) mod storage;
pub mod supervisor;
pub(crate) mod transcript;

pub use config::Config;
pub use error::AppError;
pub use service::AppService;
