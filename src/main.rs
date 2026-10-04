mod alexa;
mod browser;
mod config;
mod cookies;
mod mcp;
mod server;
mod throttle;

use alexa::AlexaClient;
use browser::BrowserClient;
use config::AppConfig;
use mcp::McpHandler;
use server::{create_router, AppState};
use std::sync::Arc;
use tokio::signal;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Default tracing filter, overridable with RUST_LOG.
const DEFAULT_LOG_FILTER: &str = "alexa_mcp=debug,tower_http=info";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let is_stdio = use_stdio_transport();
    init_tracing(is_stdio);

    info!("Starting alexa-mcp server v{}", env!("CARGO_PKG_VERSION"));

    let config = AppConfig::from_env();
    info!(
        "Configuration: bind_addr={}, amazon_url={}, cookies={}, browser_mode={:?}",
        config.bind_addr,
        config.amazon_origin(),
        config.alexa_cookies.len(),
        config.browser_mode
    );
    if config.alexa_cookies.is_empty() {
        info!("No Amazon cookies configured yet: use the alexa_get_cookies tool once logged in");
    }

    let browser_client = BrowserClient::from_config(&config);
    let alexa_client = Arc::new(AlexaClient::new(config.clone(), browser_client));
    let mcp_handler = Arc::new(McpHandler::new(alexa_client.clone()));

    if is_stdio {
        info!("Running alexa-mcp in STDIO mode");
        return run_stdio_server(mcp_handler).await;
    }

    let app_state = AppState {
        mcp_handler,
        alexa_client,
    };

    let router = create_router(app_state);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    info!("Server listening on http://{}", config.bind_addr);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shutdown cleanly");
    Ok(())
}

/// In stdio mode, log to stderr so JSON-RPC stdout stays clean.
fn init_tracing(is_stdio: bool) {
    let filter = if is_stdio {
        "alexa_mcp=info"
    } else {
        DEFAULT_LOG_FILTER
    };
    let registry = tracing_subscriber::registry().with(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
    );
    if is_stdio {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }
}

/// Returns true when the MCP stdio transport is requested.
fn use_stdio_transport() -> bool {
    std::env::args().any(|arg| arg == "--stdio")
        || std::env::var("MCP_TRANSPORT")
            .map(|value| value == "stdio")
            .unwrap_or(false)
}

/// Serves MCP JSON-RPC line by line over stdin/stdout.
async fn run_stdio_server(mcp_handler: Arc<McpHandler>) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(request) = serde_json::from_str::<crate::mcp::types::JsonRpcRequest>(trimmed) else {
            continue;
        };

        let response = mcp_handler.handle_request(request).await;
        if let Ok(json) = serde_json::to_string(&response) {
            stdout.write_all(json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
