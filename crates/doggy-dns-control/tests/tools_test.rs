#![allow(clippy::disallowed_methods)]

mod common;

use chrono::Utc;
use common::{FakeLogs, forward_plugins, test_live, test_state, test_state_with_logs};
use doggy_dns_control::server::{DoggyMcpServer, LogLevelArgs};
use doggy_dns_control::state::McpState;
use doggy_dns_control::{LogLevelAdmin, LogLevelState};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ErrorCode;
use std::sync::Arc;

/// The raw config the fixture reports; `get_config` reads it back.
const CONFIG_TEXT: &str = "[server]\nlisten_addr = \"0.0.0.0\"\nport = 15353\n";

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
    let server = DoggyMcpServer::new(
        Arc::new(test_state(forward_plugins(), CONFIG_TEXT)),
        test_live(),
    );
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
    let server = DoggyMcpServer::new(
        Arc::new(test_state(forward_plugins(), CONFIG_TEXT)),
        test_live(),
    );
    let result = server.list_plugins().await.unwrap();
    let json = first_text_json(&result);
    let plugins = json["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["kind"], "forward");
    assert_eq!(plugins[0]["order"], 0);
    assert!(plugins[0]["enabled"].as_bool().unwrap());
}

#[tokio::test]
async fn get_config_redacts_sensitive_keys() {
    let raw = r#"
[server]
listen_addr = "127.0.0.1"
port = 53

[observability.basic_auth]
username = "admin"
password = "hunter2"
"#;
    let state = McpState::from_parts(
        "h".into(),
        1,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:53".into(),
        vec!["udp".into()],
        Vec::new(),
        raw,
        FakeLogs::new("info"),
    );
    let server = DoggyMcpServer::new(Arc::new(state), test_live());
    let result = server.get_config().await.unwrap();
    let json = first_text_json(&result);

    assert_eq!(json["format"], "toml");
    let content = json["content"].as_str().unwrap();
    assert!(content.contains("admin"));
    assert!(!content.contains("hunter2"));
    assert!(content.contains("***"));
    assert!(!json["redactedKeys"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_config_returns_parse_error_for_malformed_content() {
    let state = McpState::from_parts(
        "h".into(),
        1,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:53".into(),
        vec![],
        Vec::new(),
        "this is = = not valid toml",
        FakeLogs::new("info"),
    );
    let server = DoggyMcpServer::new(Arc::new(state), test_live());
    let result = server.get_config().await.unwrap();
    let json = first_text_json(&result);
    assert_eq!(json["error"]["code"], "config_parse_error");
}

#[tokio::test]
async fn tools_declare_annotations() {
    let server = DoggyMcpServer::new(
        Arc::new(test_state(forward_plugins(), CONFIG_TEXT)),
        test_live(),
    );
    let tools = server.tool_declarations();
    assert_eq!(tools.len(), 5);

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"get_status"));
    assert!(names.contains(&"list_plugins"));
    assert!(names.contains(&"get_config"));
    assert!(names.contains(&"get_runtime_status"));
    assert!(names.contains(&"set_log_level"));

    for tool in &tools {
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(
            annotations.read_only_hint,
            Some(tool.name.as_ref() != "set_log_level"),
            "unexpected read_only_hint on {}",
            tool.name
        );
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(false));
    }
}

#[tokio::test]
async fn get_runtime_status_returns_registered_signals() {
    let server = DoggyMcpServer::new(
        Arc::new(test_state(forward_plugins(), CONFIG_TEXT)),
        test_live(),
    );
    let result = server.get_runtime_status().await.unwrap();
    let json = first_text_json(&result);
    assert_eq!(
        json["signals"]["nacos.nacos.local"]["subscribedServices"],
        3
    );
}

fn server_with_logs(logs: Arc<dyn LogLevelAdmin>) -> DoggyMcpServer {
    let state = test_state_with_logs(forward_plugins(), CONFIG_TEXT, logs);
    DoggyMcpServer::new(Arc::new(state), test_live())
}

fn log_level_args(directives: Option<&str>, reset: bool) -> Parameters<LogLevelArgs> {
    Parameters(LogLevelArgs {
        directives: directives.map(str::to_string),
        reset,
    })
}

#[tokio::test]
async fn get_runtime_status_reports_log_directives() {
    let server = server_with_logs(FakeLogs::new("info"));
    let json = first_text_json(&server.get_runtime_status().await.unwrap());
    assert_eq!(json["logs"]["stdout"], "info");
    assert!(json["logs"]["otlp"].is_null());
}

#[tokio::test]
async fn set_log_level_changes_the_filter() {
    let server = server_with_logs(FakeLogs::new("info"));

    let json = first_text_json(
        &server
            .set_log_level(log_level_args(Some("doggy_dns_nacos=debug"), false))
            .await
            .unwrap(),
    );
    assert_eq!(json["stdout"], "doggy_dns_nacos=debug");
    assert!(json["otlp"].is_null());
}

#[tokio::test]
async fn set_log_level_resets_to_the_startup_filter() {
    let server = server_with_logs(FakeLogs::new("warn"));
    server
        .set_log_level(log_level_args(Some("debug"), false))
        .await
        .unwrap();

    let json = first_text_json(
        &server
            .set_log_level(log_level_args(None, true))
            .await
            .unwrap(),
    );
    assert_eq!(json["stdout"], "warn");
}

#[tokio::test]
async fn set_log_level_rejects_bad_and_ambiguous_input() {
    let server = server_with_logs(FakeLogs::new("info"));

    let error = server
        .set_log_level(log_level_args(Some("bogus"), false))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    let error = server
        .set_log_level(log_level_args(Some("debug"), true))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    let error = server
        .set_log_level(log_level_args(None, false))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
}

/// A log control whose outputs are all disabled; `set_log_level` must refuse it.
struct NoOutputs;

impl LogLevelAdmin for NoOutputs {
    fn current(&self) -> LogLevelState {
        LogLevelState {
            stdout: None,
            otlp: None,
        }
    }

    fn set(&self, _directives: &str) -> Result<LogLevelState, String> {
        Ok(self.current())
    }

    fn reset(&self) -> Result<LogLevelState, String> {
        Ok(self.current())
    }
}

#[tokio::test]
async fn set_log_level_errors_when_no_output_is_enabled() {
    let server = server_with_logs(Arc::new(NoOutputs));
    let error = server
        .set_log_level(log_level_args(Some("debug"), false))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
}
