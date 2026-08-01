use noema::{AppService, Config, http};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "noema=info,tower_http=info".into()),
        )
        .with_target(false)
        .init();

    let config = Config::from_env()?;
    let bind = config.bind;
    let service = AppService::new(config)?;
    let app = http::router(service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "Noema HTTP and Streamable HTTP MCP service started");
    axum::serve(listener, app).await?;
    Ok(())
}
