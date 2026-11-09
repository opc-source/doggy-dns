#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_control::live::LiveState;
use doggy_dns_control::server::DoggyMcpServer;
use doggy_dns_control::state::McpState;
use std::sync::Arc;

/// The failure shape of `get_config` is the one response the other contract
/// tests pin only by error code: an `error` object carrying exactly `code` and
/// `message`, and nothing else at the top level.
#[tokio::test]
async fn config_error_body_carries_only_code_and_message() {
    let state = McpState::from_parts(
        "h".into(),
        1,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:53".into(),
        vec!["udp".into()],
        Vec::new(),
        "this is = = not valid toml",
    );
    let server = DoggyMcpServer::new(Arc::new(state), Arc::new(LiveState::new()));
    let result = server.get_config().await.unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .expect("expected text content");
    let json: serde_json::Value = serde_json::from_str(text).expect("expected valid JSON");

    let body = json.as_object().expect("body is an object");
    assert_eq!(body.len(), 1);
    let error = body["error"].as_object().expect("error is an object");
    assert_eq!(error.len(), 2);
    assert_eq!(error["code"], "config_parse_error");
    assert!(
        error["message"]
            .as_str()
            .expect("message is a string")
            .starts_with("failed to parse config: ")
    );
}
