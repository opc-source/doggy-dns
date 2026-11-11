use std::sync::Arc;

use rmcp::{
    ServerHandler,
    model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde::Serialize;

use crate::ConfigSnapshot;
use crate::PluginSnapshot;
use crate::live::LiveState;
use crate::state::McpState;

#[derive(Clone)]
pub struct DoggyMcpServer {
    state: Arc<McpState>,
    live: Arc<LiveState>,
}

/// `get_status` body.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse<'a> {
    instance_id: &'a str,
    host: &'a str,
    version: &'a str,
    pid: u32,
    uptime_seconds: u64,
    started_at: String,
    listen_addr: &'a str,
    protocols: &'a [String],
    status: &'static str,
}

/// `list_plugins` body.
#[derive(Serialize)]
struct PluginsResponse<'a> {
    plugins: &'a [PluginSnapshot],
}

/// `get_config` body for a successfully captured config.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResponse<'a> {
    format: &'static str,
    content: &'a str,
    redacted_keys: &'a [String],
}

/// `error` member of a failed tool result.
#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

/// Body returned when the config could not be captured at startup.
#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

impl DoggyMcpServer {
    pub fn new(state: Arc<McpState>, live: Arc<LiveState>) -> Self {
        Self { state, live }
    }

    pub fn tool_declarations(&self) -> Vec<rmcp::model::Tool> {
        Self::tool_router().list_all()
    }
}

#[tool_router]
impl DoggyMcpServer {
    #[tool(
        name = "get_status",
        description = "Return this doggy-dns instance's identity, version, uptime and enabled protocols.",
        annotations(
            title = "Get DNS instance status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn get_status(&self) -> Result<CallToolResult, ErrorData> {
        let now = chrono::Utc::now();
        let uptime = (now - self.state.started_at).num_seconds().max(0) as u64;
        let body = StatusResponse {
            instance_id: &self.state.instance_id,
            host: &self.state.host,
            version: &self.state.version,
            pid: self.state.pid,
            uptime_seconds: uptime,
            started_at: self.state.started_at.to_rfc3339(),
            listen_addr: &self.state.listen_addr,
            protocols: &self.state.protocols,
            status: "running",
        };
        json_text(&body)
    }

    #[tool(
        name = "list_plugins",
        description = "List all configured DNS authority plugins in evaluation order.",
        annotations(
            title = "List DNS plugins",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn list_plugins(&self) -> Result<CallToolResult, ErrorData> {
        let body = PluginsResponse {
            plugins: &self.state.plugins,
        };
        json_text(&body)
    }

    #[tool(
        name = "get_config",
        description = "Return the doggy-dns TOML configuration file with sensitive values redacted.",
        annotations(
            title = "Get redacted configuration",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn get_config(&self) -> Result<CallToolResult, ErrorData> {
        match &self.state.config {
            ConfigSnapshot::Redacted {
                content,
                redacted_keys,
            } => json_text(&ConfigResponse {
                format: "toml",
                content,
                redacted_keys,
            }),
            ConfigSnapshot::Error { code, message } => json_text(&ErrorBody {
                error: ErrorDetail { code, message },
            }),
        }
    }

    #[tool(
        name = "get_runtime_status",
        description = "Return live runtime signals (per-zone subscription counts, fast-path state) sampled at call time.",
        annotations(
            title = "Get live runtime signals",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn get_runtime_status(&self) -> Result<CallToolResult, ErrorData> {
        json_text(&self.live.snapshot())
    }
}

fn json_text<T: Serialize>(body: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(body).expect("tool response is serializable");
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

#[tool_handler(name = "doggy-dns-control", version = "0.1.0")]
impl ServerHandler for DoggyMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "doggy-dns-control",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Read-only introspection of a doggy-dns instance.")
    }
}
