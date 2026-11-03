#![allow(clippy::disallowed_methods)]

//! Kept in its own test binary on purpose. `set_default` installs a scoped
//! subscriber, but tracing caches each callsite's interest process-wide, and
//! `NoSubscriber` caches `Interest::never`. A test in the same binary that
//! emits the same event without a subscriber can therefore switch the callsite
//! off for this one mid-run; a binary with a single test cannot race that way.

use doggy_dns_core::middleware::logging::{LoggingMiddleware, LoggingToggle};
use doggy_dns_plugin::{Middleware, RequestOutcome};
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::Request;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

fn make_dns_request(domain: &str, rtype: RecordType) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;

    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);

    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

/// Query logging is reloadable, so the toggle must actually start and stop
/// the log lines — not merely remember a bool.
///
/// `set_default` installs a thread-local subscriber, so the test pins the
/// current-thread runtime: the guard and the awaited calls stay on one thread.
#[tokio::test(flavor = "current_thread")]
async fn logging_toggle_controls_emitted_lines() {
    let request = make_dns_request("example.com.", RecordType::A);
    let outcome = RequestOutcome::success("native".to_string(), 1, None, None);

    let buffer = LogBuffer::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish(),
    );

    let toggle = LoggingToggle::new(true);
    let mw = LoggingMiddleware::with_toggle(toggle.clone());
    mw.before_request(&request).await;
    mw.after_request(&request, 42, &outcome).await;
    let enabled_lines = buffer.take();

    toggle.set(false);
    mw.before_request(&request).await;
    mw.after_request(&request, 42, &outcome).await;
    let disabled_lines = buffer.take();

    toggle.set(true);
    mw.before_request(&request).await;
    let reenabled_lines = buffer.take();

    drop(guard);

    assert!(
        enabled_lines.contains("dns query received")
            && enabled_lines.contains("dns query completed"),
        "an enabled toggle must log both halves of the request: {enabled_lines}"
    );
    assert!(
        disabled_lines.is_empty(),
        "a disabled toggle must log nothing: {disabled_lines}"
    );
    assert!(
        reenabled_lines.contains("dns query received"),
        "switching back on mid-process must log again: {reenabled_lines}"
    );
}

/// In-memory `MakeWriter` the fmt layer can write into from the test thread.
#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl LogBuffer {
    fn take(&self) -> String {
        let mut bytes = self.0.lock().unwrap();
        String::from_utf8(std::mem::take(&mut *bytes)).unwrap()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
