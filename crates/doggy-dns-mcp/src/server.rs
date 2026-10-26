use std::sync::Arc;

use rmcp::{
    ServerHandler,
    model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde_json::Value;

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
        let mut map = serde_json::Map::new();
        map.insert(
            "instanceId".into(),
            Value::String(self.state.instance_id.clone()),
        );
        map.insert("host".into(), Value::String(self.state.host.clone()));
        map.insert("version".into(), Value::String(self.state.version.clone()));
        map.insert("pid".into(), Value::from(self.state.pid));
        map.insert("uptimeSeconds".into(), Value::from(uptime));
        map.insert(
            "startedAt".into(),
            Value::String(self.state.started_at.to_rfc3339()),
        );
        map.insert(
            "listenAddr".into(),
            Value::String(self.state.listen_addr.clone()),
        );
        map.insert(
            "protocols".into(),
            Value::Array(
                self.state
                    .protocols
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            ),
        );
        map.insert("status".into(), Value::String("running".into()));
        Ok(CallToolResult::success(vec![ContentBlock::text(
            Value::Object(map).to_string(),
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
        let plugins: Vec<Value> = self
            .state
            .plugins
            .iter()
            .map(|p| serde_json::to_value(p).expect("PluginSnapshot is serializable"))
            .collect();
        let mut map = serde_json::Map::new();
        map.insert("plugins".into(), Value::Array(plugins));
        Ok(CallToolResult::success(vec![ContentBlock::text(
            Value::Object(map).to_string(),
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
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    error_json(
                        "config_unreadable",
                        &format!("failed to read config file: {e}"),
                    ),
                )]));
            }
        };

        match redact_toml(&content) {
            Ok((redacted_content, redacted_keys)) => {
                let mut map = serde_json::Map::new();
                map.insert("format".into(), Value::String("toml".into()));
                map.insert("content".into(), Value::String(redacted_content));
                map.insert(
                    "redactedKeys".into(),
                    Value::Array(redacted_keys.into_iter().map(Value::String).collect()),
                );
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    Value::Object(map).to_string(),
                )]))
            }
            Err(e) => Ok(CallToolResult::success(vec![ContentBlock::text(
                error_json(
                    "config_parse_error",
                    &format!("failed to parse config: {e}"),
                ),
            )])),
        }
    }
}

fn error_json(code: &str, message: &str) -> String {
    let mut error = serde_json::Map::new();
    error.insert("code".into(), Value::String(code.into()));
    error.insert("message".into(), Value::String(message.into()));
    let mut map = serde_json::Map::new();
    map.insert("error".into(), Value::Object(error));
    Value::Object(map).to_string()
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
