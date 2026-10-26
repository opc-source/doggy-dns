#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_mcp::server::DoggyMcpServer;
use doggy_dns_mcp::state::McpState;
use doggy_dns_mcp::PluginSnapshot;
use std::path::PathBuf;
use std::sync::Arc;

fn test_state() -> McpState {
    McpState::from_parts(
        "dns-node-1".into(),
        4242,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        vec![PluginSnapshot {
            order: 0,
            kind: "forward".into(),
            enabled: true,
            details: serde_json::json!({"upstream": ["8.8.8.8:53"]}),
        }],
        PathBuf::from("config/doggy-dns.toml"),
    )
}

fn first_text_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .expect("expected text content");
    serde_json::from_str(text).expect("expected valid JSON")
}

#[tokio::test]
async fn get_status_returns_identity_and_uptime() {
    let server = DoggyMcpServer::new(Arc::new(test_state()));
    let result = server.get_status().await.unwrap();
    let json = first_text_json(&result);
    assert_eq!(json["instanceId"], "dns-node-1-4242");
    assert_eq!(json["host"], "dns-node-1");
    assert_eq!(json["version"], "0.1.0");
    assert_eq!(json["pid"], 4242);
    assert_eq!(json["listenAddr"], "0.0.0.0:15353");
    assert_eq!(json["protocols"][0], "udp");
    assert_eq!(json["protocols"][1], "tcp");
    assert_eq!(json["status"], "running");
    assert!(json["uptimeSeconds"].as_u64().is_some());
    assert!(json["startedAt"].as_str().is_some());
}

#[tokio::test]
async fn list_plugins_returns_snapshots() {
    let server = DoggyMcpServer::new(Arc::new(test_state()));
    let result = server.list_plugins().await.unwrap();
    let json = first_text_json(&result);
    let plugins = json["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["kind"], "forward");
    assert_eq!(plugins[0]["order"], 0);
    assert!(plugins[0]["enabled"].as_bool().unwrap());
}

#[tokio::test]
async fn get_config_reads_and_redacts_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("test.toml");
    std::fs::write(
        &config_path,
        r#"
[server]
listen_addr = "127.0.0.1"
port = 53

[observability.basic_auth]
username = "admin"
password = "hunter2"
"#,
    )
    .unwrap();

    let state = McpState::from_parts(
        "h".into(),
        1,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:53".into(),
        vec!["udp".into()],
        Vec::new(),
        config_path,
    );
    let server = DoggyMcpServer::new(Arc::new(state));
    let result = server.get_config().await.unwrap();
    let json = first_text_json(&result);

    assert_eq!(json["format"], "toml");
    let content = json["content"].as_str().unwrap();
    assert!(content.contains("admin"));
    assert!(!content.contains("hunter2"));
    assert!(content.contains("***"));
    assert!(json["redactedKeys"].as_array().unwrap().len() >= 1);
}

#[tokio::test]
async fn get_config_returns_error_for_missing_file() {
    let state = McpState::from_parts(
        "h".into(),
        1,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:53".into(),
        vec![],
        Vec::new(),
        PathBuf::from("/nonexistent/config.toml"),
    );
    let server = DoggyMcpServer::new(Arc::new(state));
    let result = server.get_config().await.unwrap();
    let json = first_text_json(&result);
    assert_eq!(json["error"]["code"], "config_unreadable");
}

#[tokio::test]
async fn tools_declare_read_only_annotations() {
    let server = DoggyMcpServer::new(Arc::new(test_state()));
    let tools = server.tool_declarations();
    assert_eq!(tools.len(), 3);

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"get_status"));
    assert!(names.contains(&"list_plugins"));
    assert!(names.contains(&"get_config"));

    for tool in &tools {
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(false));
    }
}
