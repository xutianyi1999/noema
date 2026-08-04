//! noema: the OpenCode-driven text knowledge-base service (server binary).
//!
//! Serves the HTTP JSON API and the Streamable HTTP MCP endpoint. Every
//! setting falls back from flag to environment variable to built-in default
//! (`noema --help`). Administration happens through the HTTP API — see the
//! `noema-cli` client binary, which also works from remote machines.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use noema::{http, AppError, AppService, Config};
use opencode_rs::server::{ManagedServer, ServerOptions};

/// OpenCode 驱动的文本知识库服务：HTTP JSON API + Streamable HTTP MCP。
///
/// 命令行客户端为 noema-cli（所有操作经本服务 HTTP API，可远程管理）。内容库之间完全隔离，互不共享文件。
#[derive(Parser)]
#[command(name = "noema", version, about)]
struct Cli {
    /// 监听地址
    #[arg(long, env = "NOEMA_BIND", default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    /// 数据目录（控制库 control.sqlite 和所有内容库）
    #[arg(long, env = "NOEMA_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// 模型标识
    #[arg(
        long,
        env = "OPENCODE_MODEL",
        value_name = "MODEL",
        default_value = "opencode/deepseek-v4-flash-free"
    )]
    model: String,
    /// 单个 Agent session 超时（秒）
    #[arg(long, env = "OPENCODE_TIMEOUT_SECS", default_value_t = 1800)]
    opencode_timeout_secs: u64,
    /// 已由容器入口启动的 OpenCode Server 地址；省略时由 Noema 自行拉起子进程。
    #[arg(long, env = "NOEMA_OPENCODE_URL")]
    opencode_url: Option<String>,
    /// 全局并发 Agent session 上限（摄入与查询之和），超出的请求排队等待
    #[arg(long, env = "NOEMA_MAX_SESSIONS", default_value_t = 4)]
    max_sessions: usize,
    /// HTTP API 的 Bearer 令牌。设置后除 /v1/health 外所有路由（含 MCP 端点）
    /// 强制校验 Authorization: Bearer <token>；缺省保持无鉴权，仅适合绑定 loopback
    #[arg(long, env = "NOEMA_AUTH_TOKEN")]
    auth_token: Option<String>,
    /// 流式打印 OpenCode 会话的中间过程（仅服务端日志；接口始终只返回最终答案）。
    /// 裸 `--transcript` 即开启，也可带值 `--transcript=false`。
    #[arg(
        long,
        env = "NOEMA_TRANSCRIPT",
        value_name = "true|false",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    transcript: bool,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "noema=info".into()))
        .with_target(false)
        .init();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("noema: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(serve(cli))
}

async fn serve(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.max_sessions == 0 {
        return Err("--max-sessions must be at least 1".into());
    }
    if cli.opencode_timeout_secs == 0 {
        return Err("--opencode-timeout-secs must be at least 1".into());
    }
    if cli.auth_token.as_deref() == Some("") {
        return Err(
            "--auth-token must not be empty: an empty token would authenticate empty bearer headers"
                .into(),
        );
    }
    let bind = cli.bind;
    if !bind.ip().is_loopback() && cli.auth_token.is_none() {
        tracing::warn!(
            %bind,
            "binding a non-loopback address without --auth-token; the HTTP API is unauthenticated"
        );
    }
    // Register the signal handlers BEFORE the child process and the
    // startup maintenance run: a SIGTERM/SIGINT arriving during that
    // window must not kill the process with the default handler, which
    // would orphan the managed OpenCode child. Once handlers are
    // installed, an early signal simply queues until the graceful
    // shutdown below starts.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let configured_opencode_url = cli
        .opencode_url
        .map(|url| url.trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty());
    let managed = if configured_opencode_url.is_some() {
        tracing::info!(url = ?configured_opencode_url, "using externally managed OpenCode Server");
        None
    } else {
        Some(spawn_opencode_server().await?)
    };
    let opencode_url = configured_opencode_url
        .or_else(|| managed.as_ref().map(|server| server.url().to_string()))
        .expect("managed or configured OpenCode URL");
    let config = Config {
        data_dir: cli.data_dir,
        // Noema uses either the container-shared server or its managed child.
        opencode_url,
        opencode_model: cli.model,
        opencode_timeout_secs: cli.opencode_timeout_secs,
        transcript: cli.transcript,
        max_sessions: cli.max_sessions,
        auth_token: cli.auth_token,
    };
    let service = AppService::new(config)?;
    let app = http::router(service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "Noema HTTP and Streamable HTTP MCP service started");

    // Graceful shutdown: on a signal, axum finishes the in-flight requests
    // (queries complete instead of dying mid-session) before the managed
    // OpenCode server is stopped. Detached ingest tasks are still cut off
    // by the runtime drop; the startup reap marks them failed on next boot.
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, stopping gracefully"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM, stopping gracefully"),
        }
    };
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(Into::into);

    if let Some(managed) = managed {
        if let Err(error) = managed.stop().await {
            tracing::warn!(%error, "failed to stop OpenCode Server");
        }
    }
    result
}

/// How long the spawned OpenCode Server may take to start accepting
/// connections.
const STARTUP_TIMEOUT_MS: u64 = 60_000;

/// Spawn `opencode serve` on a free local port and wait until it is ready.
///
/// The child is fully owned by the returned [`ManagedServer`]: dropping it
/// terminates the process group, and [`ManagedServer::stop`] shuts it down
/// gracefully.
async fn spawn_opencode_server() -> Result<ManagedServer, AppError> {
    let options = ServerOptions::new().startup_timeout_ms(STARTUP_TIMEOUT_MS);
    tracing::info!("spawning OpenCode Server");
    let managed = ManagedServer::start(options).await?;
    tracing::info!(url = %managed.url(), "OpenCode Server is ready");
    Ok(managed)
}
