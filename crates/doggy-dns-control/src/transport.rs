use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Json},
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::ControlConfig;
use crate::live::LiveState;
use crate::server::DoggyMcpServer;
use crate::state::McpState;

/// Handle to a running control plane listener.
#[derive(Debug)]
pub struct ControlPlaneHandle {
    pub addr: SocketAddr,
    pub cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl ControlPlaneHandle {
    /// Wait for the server to finish.
    pub async fn wait(self) -> Result<(), tokio::task::JoinError> {
        self.join.await
    }
}

/// Start the control plane listener on the configured address and port.
///
/// Which routes exist is decided by `cfg.endpoints`: `mcp` nests the MCP
/// JSON-RPC service at `/mcp` (guarded by the Bearer token when set),
/// while `healthz` and `readyz` are plain GET probes that never require
/// auth. Returns a handle that can be used to shut the listener down.
/// If the port is already in use, returns an error.
pub async fn start(
    state: Arc<McpState>,
    live: Arc<LiveState>,
    cfg: ControlConfig,
) -> anyhow::Result<ControlPlaneHandle> {
    let cancel = CancellationToken::new();
    let addr = doggy_dns_basic::net::socket_addr(&cfg.listen_addr, cfg.port)?;
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;

    let router = build_router(state, &live, &cfg, cancel.clone());

    let cancel_for_shutdown = cancel.clone();
    let join = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                cancel_for_shutdown.cancelled().await;
            })
            .await
            .ok();
    });

    Ok(ControlPlaneHandle { addr, cancel, join })
}

/// Build the axum router for the control plane, exposing only the
/// endpoints selected in configuration.
pub fn build_router(
    state: Arc<McpState>,
    live: &Arc<LiveState>,
    cfg: &ControlConfig,
    cancel: CancellationToken,
) -> Router {
    let mut router = Router::new();

    if has_endpoint(cfg, "mcp") {
        let server_config = StreamableHttpServerConfig::default()
            .with_cancellation_token(cancel)
            .with_allowed_hosts(cfg.allowed_hosts.clone());

        let state_for_factory = state.clone();
        let live_for_factory = live.clone();
        let service = StreamableHttpService::new(
            move || {
                Ok(DoggyMcpServer::new(
                    state_for_factory.clone(),
                    live_for_factory.clone(),
                ))
            },
            Arc::new(LocalSessionManager::default()),
            server_config,
        );

        let mut mcp_routes = Router::new().nest_service("/mcp", service);
        if let Some(token) = &cfg.token {
            let expected = format!("Bearer {token}");
            mcp_routes = mcp_routes.layer(middleware::from_fn(
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
        router = router.merge(mcp_routes);
    }

    if has_endpoint(cfg, "healthz") {
        router = router.route("/healthz", get(healthz));
    }
    if has_endpoint(cfg, "readyz") {
        let live_for_readyz = live.clone();
        router = router.route("/readyz", get(move || readyz(live_for_readyz.clone())));
    }

    router
}

fn has_endpoint(cfg: &ControlConfig, name: &str) -> bool {
    cfg.endpoints.iter().any(|e| e == name)
}

/// Liveness: the process is up and this route is wired.
async fn healthz() -> Json<Value> {
    let mut body = serde_json::Map::new();
    body.insert("status".to_string(), Value::from("ok"));
    Json(Value::Object(body))
}

/// Readiness as a pure report: always 200, live signals in the body.
/// Choosing what counts as "not ready" is the operator's (or
/// doggy-dns-console's) call, not this server's.
async fn readyz(live: Arc<LiveState>) -> Json<Value> {
    Json(live.snapshot())
}
