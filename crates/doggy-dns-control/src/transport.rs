use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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
/// If the port is already in use, returns an error — see [`start_retrying`],
/// which the binary uses to wait for the port instead of giving up on it.
pub async fn start(
    state: Arc<McpState>,
    live: Arc<LiveState>,
    cfg: ControlConfig,
) -> anyhow::Result<ControlPlaneHandle> {
    let cancel = CancellationToken::new();
    let addr = doggy_dns_basic::net::socket_addr(&cfg.listen_addr, cfg.port)?;
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;

    let serve_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        serve(listener, state, live, &cfg, serve_cancel).await;
    });

    Ok(ControlPlaneHandle { addr, cancel, join })
}

/// How long a successor waits before it tries the control port again. The
/// predecessor it is waiting behind drains in `shutdown_timeout` seconds, so
/// one or two attempts are normally enough.
pub const BIND_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// Start the control plane and hold the port until it is free.
///
/// A restart overlaps its predecessor on the DNS port because every DNS socket
/// binds with `SO_REUSEPORT`. The control port is deliberately not armed the
/// same way: two processes serving `/readyz` for one node would report two
/// live instances to the console, so the successor waits for the predecessor to
/// release the address and re-attempts the bind every [`BIND_RETRY_INTERVAL`].
///
/// This returns before the port is bound, which is why the address it carries is
/// the configured one; the real bound address — differing when `port = 0` — is
/// logged by the retrying task itself. Cancelling the handle stops the waiting,
/// and `retry_in` is a parameter rather than [`BIND_RETRY_INTERVAL`] so a test
/// can watch a retry land without waiting out ten seconds.
///
/// `dns_overlap` is the caller's `DNS_PORT_OVERLAP_SUPPORTED`, reported with
/// every failed attempt: on a platform where the DNS sockets do overlap, a held
/// control port right after a restart is the expected race, and anywhere else it
/// means some other process owns the address. It is passed in rather than read
/// from `doggy-dns-core` because this crate deliberately does not depend on it.
pub fn start_retrying(
    state: Arc<McpState>,
    live: Arc<LiveState>,
    cfg: ControlConfig,
    retry_in: Duration,
    dns_overlap: bool,
) -> anyhow::Result<ControlPlaneHandle> {
    let cancel = CancellationToken::new();
    let addr = doggy_dns_basic::net::socket_addr(&cfg.listen_addr, cfg.port)?;

    let wait_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        // Warned on every attempt, not just the first: the line is the record
        // that the control plane is still dark, and it stops appearing the
        // moment the bind lands — so its absence says more than one line would.
        let listener = loop {
            match TcpListener::bind(addr).await {
                Ok(listener) => break listener,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        addr = %addr,
                        dns_overlap,
                        "control port still held; retrying the bind"
                    );
                    tokio::select! {
                        _ = wait_cancel.cancelled() => return,
                        _ = tokio::time::sleep(retry_in) => {}
                    }
                }
            }
        };

        tracing::info!(addr = %listener.local_addr().unwrap_or(addr), "control plane listening");
        serve(listener, state, live, &cfg, wait_cancel).await;
    });

    Ok(ControlPlaneHandle { addr, cancel, join })
}

/// Serve the router over an already bound listener until `cancel` fires.
async fn serve(
    listener: TcpListener,
    state: Arc<McpState>,
    live: Arc<LiveState>,
    cfg: &ControlConfig,
    cancel: CancellationToken,
) {
    let router = build_router(state, &live, cfg, cancel.clone());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .ok();
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
