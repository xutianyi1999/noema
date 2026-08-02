//! Managed OpenCode Server child process: spawned via the opencode_rs SDK
//! when the service starts, stopped together with it. The server takes a
//! free port on 127.0.0.1; its actual URL is discovered from the returned
//! handle, so nothing about the address is user-configurable.

use opencode_rs::server::{ManagedServer, ServerOptions};

use crate::error::AppError;

/// How long the spawned server may take to start accepting connections.
const STARTUP_TIMEOUT_MS: u64 = 60_000;

/// Spawn `opencode serve` on a free local port and wait until it is ready.
///
/// The child is fully owned by the returned [`ManagedServer`]: dropping it
/// terminates the process group, and [`ManagedServer::stop`] shuts it down
/// gracefully.
pub async fn spawn() -> Result<ManagedServer, AppError> {
    let options = ServerOptions::new().startup_timeout_ms(STARTUP_TIMEOUT_MS);
    tracing::info!("spawning OpenCode Server");
    let managed = ManagedServer::start(options).await?;
    tracing::info!(url = %managed.url(), "OpenCode Server is ready");
    Ok(managed)
}
