#![allow(clippy::disallowed_methods)]

use doggy_dns_core::config::DoggyDnsConfig;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_core::server::build_server;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use std::sync::Arc;

fn handler() -> DoggyDnsHandler {
    DoggyDnsHandler::new(
        Vec::new(),
        Arc::new(AuthorityChain::new(Vec::new())),
        false,
        AnswerRotation::new(false),
    )
}

#[tokio::test]
async fn build_server_binds_ipv6_wildcard_listen_addr() {
    let config: DoggyDnsConfig = toml::from_str(
        r#"
[server]
listen_addr = "::"
port = 0
"#,
    )
    .expect("parse failed");

    build_server(&config, handler())
        .await
        .expect("IPv6 listen address should bind");
}

#[tokio::test]
async fn build_server_binds_bracketed_ipv6_listen_addr() {
    let config: DoggyDnsConfig = toml::from_str(
        r#"
[server]
listen_addr = "[::1]"
port = 0
"#,
    )
    .expect("parse failed");

    build_server(&config, handler())
        .await
        .expect("bracketed IPv6 listen address should bind");
}
