use std::sync::Arc;

use rmcp::{
    ServerHandler,
    model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};

use crate::redact::redact_toml;
use crate::state::McpState;

#[derive(Clone)]
pub struct DoggyMcpServer {
    state: Arc<McpState>,
}

impl DoggyMcpServer {
    pub fn new(state: Arc<McpState>) -> Self {
        Self { state }
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
        let json = serde_json::json!({
            "instanceId": self.state.instance_id,
            "host": self.state.host,
            "version": self.state.version,
            "pid": self.state.pid,
            "uptimeSeconds": uptime,
            "startedAt": self.state.started_at.to_rfc3339(),
            "listenAddr": self.state.listen_addr,
            "protocols": self.state.protocols,
            "status": "running",
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            json.to_string(),
        )]))
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
        let json = serde_json::json!({
            "plugins": self.state.plugins,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            json.to_string(),
        )]))
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
        let content = match std::fs::read_to_string(&self.state.config_path) {
            Ok(c) => c,
            Err(e) => {
                let json = serde_json::json!({
                    "error": {
                        "code": "config_unreadable",
                        "message": format!("failed to read config file: {e}"),
                    }
                });
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    json.to_string(),
                )]));
            }
        };

        match redact_toml(&content) {
            Ok((redacted_content, redacted_keys)) => {
                let json = serde_json::json!({
                    "format": "toml",
                    "content": redacted_content,
                    "redactedKeys": redacted_keys,
                });
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    json.to_string(),
                )]))
            }
            Err(e) => {
                let json = serde_json::json!({
                    "error": {
                        "code": "config_parse_error",
                        "message": format!("failed to parse config: {e}"),
                    }
                });
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    json.to_string(),
                )]))
            }
        }
    }
}

#[tool_handler(name = "doggy-dns-mcp", version = "0.1.0")]
impl ServerHandler for DoggyMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "doggy-dns-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Read-only introspection of a doggy-dns instance.")
    }
}
