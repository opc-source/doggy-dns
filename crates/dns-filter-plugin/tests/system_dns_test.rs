#![allow(clippy::disallowed_methods)]

use dns_filter_plugin::builtin::system_dns::SystemAuthority;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::time::Duration;

#[tokio::test]
async fn system_dns_resolves_known_domain() {
    let authority = SystemAuthority::new(256, Duration::from_secs(10), Duration::from_secs(300))
        .await
        .expect("failed to create SystemAuthority");

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_)) => {
            // Success
        }
        _ => panic!("Expected successful lookup, got non-success result"),
    }
}
