#![allow(clippy::disallowed_methods)]

//! Pins how the handler renders its tracing fields.
//!
//! Kept in its own test target: tracing caches callsite interest process-wide,
//! so another test hitting the same event first can silence it before this
//! subscriber is installed.

mod common;

use common::dns_request;
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::CachedInstance;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_testutil::CapturingHandler;
use hickory_proto::rr::RecordType;
use hickory_server::net::runtime::TokioTime;
use hickory_server::server::RequestHandler;
use std::sync::Arc;

/// Captures formatted log output so the test can assert the rendered values.
#[derive(Clone)]
struct SharedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedLogs {
    fn take(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("log output is UTF-8")
    }
}

type MapCatalog = DashMap<String, Vec<CachedInstance>>;

fn hit_authority() -> NacosAuthority<MapCatalog> {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "my-svc".to_string(),
        vec![CachedInstance {
            ip: "10.0.0.1".parse().unwrap(),
            port: 8080,
            healthy: true,
        }],
    );
    NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 60)
}

#[tokio::test]
async fn handler_renders_the_same_log_field_values_as_before() {
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(hit_authority()))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));
    let request = dns_request(
        1234,
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        None,
    );

    let logs = SharedLogs(Arc::new(std::sync::Mutex::new(Vec::new())));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    handler
        .handle_request::<_, TokioTime>(&request, CapturingHandler::default())
        .await;
    drop(guard);

    let text = logs.take();
    assert!(
        text.contains("authority=\"nacos\""),
        "the event keeps the bare &str rendering: {text}"
    );
    assert!(
        text.contains("dns.authority=nacos"),
        "the span keeps the unquoted display rendering: {text}"
    );
    assert!(
        text.contains("network.protocol.name=udp"),
        "the transport field keeps its lowercase name: {text}"
    );
    assert!(
        text.contains("dns.response.code=NOERROR"),
        "the response code keeps its uppercase mnemonic: {text}"
    );
}
