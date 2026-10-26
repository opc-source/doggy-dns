use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    http::{HeaderMap, StatusCode},
    middleware,
    response::IntoResponse,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::McpConfig;
use crate::server::DoggyMcpServer;
use crate::state::McpState;

/// Handle to a running MCP server.
#[derive(Debug)]
pub struct McpServerHandle {
    pub addr: SocketAddr,
    pub cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl McpServerHandle {
    /// Wait for the server to finish.
    pub async fn wait(self) -> Result<(), tokio::task::JoinError> {
        self.join.await
    }
}

/// Start the MCP server on the configured address and port.
///
/// Returns a handle that can be used to shut down the server.
/// If the port is already in use, returns an error.
pub async fn start(state: Arc<McpState>, cfg: McpConfig) -> anyhow::Result<McpServerHandle> {
    let cancel = CancellationToken::new();
    let listener = TcpListener::bind(format!("{}:{}", cfg.listen_addr, cfg.port)).await?;
    let addr = listener.local_addr()?;

    let router = build_router(state, &cfg, cancel.clone());

    let cancel_for_shutdown = cancel.clone();
    let join = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                cancel_for_shutdown.cancelled().await;
            })
            .await
            .ok();
    });

    Ok(McpServerHandle { addr, cancel, join })
}

/// Build the axum router for the MCP server.
pub fn build_router(state: Arc<McpState>, cfg: &McpConfig, cancel: CancellationToken) -> Router {
    let server_config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancel)
        .with_allowed_hosts(cfg.allowed_hosts.clone());

    let state_for_factory = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(DoggyMcpServer::new(state_for_factory.clone())),
        Arc::new(LocalSessionManager::default()),
        server_config,
    );

    let mut router = Router::new().nest_service("/mcp", service);

    if let Some(token) = &cfg.token {
        let expected = format!("Bearer {token}");
        router = router.layer(middleware::from_fn(
            move |headers: HeaderMap, req: axum::extract::Request, next: middleware::Next| {
                let expected = expected.clone();
                async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok());
                    match auth {
                        Some(value) if value == expected => next.run(req).await,
                        _ => (StatusCode::UNAUTHORIZED, r#"{"error":"unauthorized"}"#)
                            .into_response(),
                    }
                }
            },
        ));
    }

    router
}
