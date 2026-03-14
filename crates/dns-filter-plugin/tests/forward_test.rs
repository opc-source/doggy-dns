#![allow(clippy::disallowed_methods)]

use dns_filter_plugin::builtin::forward::ForwardAuthority;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::time::Duration;

#[tokio::test]
async fn forward_resolves_known_domain() {
    let authority = ForwardAuthority::new(
        vec!["8.8.8.8:53".to_string()],
        1024,
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await
    .expect("failed to create ForwardAuthority");

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_)) => {
            // Success: resolved
        }
        _ => panic!("Expected successful lookup, got non-success result"),
    }
}
