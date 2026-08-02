//! noema: the OpenCode-driven text knowledge-base service (server binary).
//!
//! Serves the HTTP JSON API and the Streamable HTTP MCP endpoint. Every
//! setting falls back from flag to environment variable to built-in default
//! (`noema --help`). Administration happens through the HTTP API — see the
//! `noema-cli` client binary, which also works from remote machines.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use noema::{AppService, Config, http};

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
    /// 全局并发 Agent session 上限（摄入与查询之和），超出的请求排队等待
    #[arg(long, env = "NOEMA_MAX_SESSIONS", default_value_t = 4)]
    max_sessions: usize,
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
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "noema=info,tower_http=info".into()),
        )
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
    let bind = cli.bind;
    let managed = noema::supervisor::spawn().await?;
    let config = Config {
        data_dir: cli.data_dir,
        bind,
        // 客户端连接始终指向刚刚拉起的这个实例。
        opencode_url: managed.url().to_string(),
        opencode_model: cli.model,
        opencode_timeout_secs: cli.opencode_timeout_secs,
        transcript: cli.transcript,
        max_sessions: cli.max_sessions,
    };
    let service = AppService::new(config)?;
    let app = http::router(service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "Noema HTTP and Streamable HTTP MCP service started");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Into::into),
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received SIGINT, stopping");
            Ok(())
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, stopping");
            Ok(())
        }
    };

    if let Err(error) = managed.stop().await {
        tracing::warn!(%error, "failed to stop OpenCode Server");
    }
    result
}
