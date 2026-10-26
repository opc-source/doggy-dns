#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_mcp::state::McpState;
use doggy_dns_mcp::transport;
use doggy_dns_mcp::{McpConfig, PluginSnapshot};
use std::sync::Arc;

fn test_state() -> Arc<McpState> {
    Arc::new(McpState::from_parts(
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
        "",
    ))
}

fn initialize_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "0.1.0"
            }
        }
    })
}

fn tools_list_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    })
}

async fn post_mcp(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    body: &serde_json::Value,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    req.send().await.unwrap()
}

async fn post_mcp_with_session(
    addr: std::net::SocketAddr,
    session_id: &str,
    body: &serde_json::Value,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn lists_three_read_only_tools_and_calls_status() {
    let state = test_state();
    let cfg = McpConfig {
        port: 0,
        token: None,
        ..Default::default()
    };
    let handle = transport::start(state, cfg).await.unwrap();

    let resp = post_mcp(handle.addr, None, &initialize_request()).await;
    assert_eq!(resp.status(), 200);
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());

    let tools_req = tools_list_request();
    let resp = if let Some(ref sid) = session_id {
        post_mcp_with_session(handle.addr, sid, &tools_req).await
    } else {
        post_mcp(handle.addr, None, &tools_req).await
    };
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    let json: serde_json::Value = parse_sse_or_json(&body);
    let tools = json["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"get_status"));
    assert!(names.contains(&"list_plugins"));
    assert!(names.contains(&"get_config"));

    for tool in tools {
        let annotations = tool["annotations"].as_object().unwrap();
        assert_eq!(annotations["readOnlyHint"], true);
        assert_eq!(annotations["destructiveHint"], false);
    }

    handle.cancel.cancel();
}

#[tokio::test]
async fn rejects_missing_or_wrong_token() {
    let state = test_state();
    let cfg = McpConfig {
        port: 0,
        token: Some("s3cret".into()),
        ..Default::default()
    };
    let handle = transport::start(state, cfg).await.unwrap();

    let resp = post_mcp(handle.addr, None, &initialize_request()).await;
    assert_eq!(resp.status(), 401);

    let resp = post_mcp(handle.addr, Some("wrong"), &initialize_request()).await;
    assert_eq!(resp.status(), 401);

    let resp = post_mcp(handle.addr, Some("s3cret"), &initialize_request()).await;
    assert_eq!(resp.status(), 200);

    handle.cancel.cancel();
}

#[tokio::test]
async fn initialize_returns_doggy_dns_mcp_server_info() {
    let state = test_state();
    let cfg = McpConfig {
        port: 0,
        token: None,
        ..Default::default()
    };
    let handle = transport::start(state, cfg).await.unwrap();

    let resp = post_mcp(handle.addr, None, &initialize_request()).await;
    let body = resp.text().await.unwrap();
    let json: serde_json::Value = parse_sse_or_json(&body);
    let server_info = &json["result"]["serverInfo"];
    assert_eq!(server_info["name"], "doggy-dns-mcp");

    handle.cancel.cancel();
}

fn parse_sse_or_json(body: &str) -> serde_json::Value {
    if let Ok(json) = serde_json::from_str(body) {
        return json;
    }
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(json) = serde_json::from_str(data.trim())
        {
            return json;
        }
    }
    panic!("could not parse response as JSON or SSE: {body}")
}
