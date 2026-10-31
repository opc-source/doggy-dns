#![allow(dead_code)]
#![allow(clippy::disallowed_methods)]

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::Request;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use doggy_dns_basic::{
    ControlConfig, DoggyDnsConfig, MiddlewareConfig, ObservabilityConfig, RemoteConfig,
    ServerConfig, XdpConfig,
};

pub fn full_config(observability: ObservabilityConfig) -> DoggyDnsConfig {
    DoggyDnsConfig {
        server: ServerConfig {
            listen_addr: "127.0.0.1".to_string(),
            port: 53,
            tcp_timeout: 10,
            shutdown_timeout: 5,
            worker_threads: 1,
            rotate_answers: true,
            tls: None,
            https: None,
        },
        middleware: MiddlewareConfig::default(),
        plugins: Vec::new(),
        remote_config: RemoteConfig::default(),
        observability,
        xdp: XdpConfig::default(),
        control: ControlConfig::default(),
    }
}

pub struct CapturedRequest {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub async fn mock_otlp_server() -> (SocketAddr, UnboundedReceiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (sender, receiver) = unbounded_channel();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let sender = sender.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 8192];

                loop {
                    let header_end = loop {
                        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break position;
                        }
                        let read = match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => read,
                        };
                        buffer.extend_from_slice(&chunk[..read]);
                    };

                    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                    let path = headers
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    let mut parsed_headers = HashMap::new();
                    for line in headers.lines().skip(1) {
                        if let Some((name, value)) = line.split_once(':') {
                            parsed_headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }
                    let content_length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);

                    let body_start = header_end + 4;
                    while buffer.len() < body_start + content_length {
                        let read = match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => read,
                        };
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let body_end = body_start + content_length;
                    let body = buffer[body_start..body_end].to_vec();
                    buffer.drain(..body_end);

                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await;
                    let _ = sender.send(CapturedRequest {
                        path,
                        headers: parsed_headers,
                        body,
                    });
                }
            });
        }
    });

    (addr, receiver)
}

pub fn resource_attribute(
    attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue],
    key: &str,
) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(value) => {
                Some(value.clone())
            }
            _ => None,
        })
}

pub fn make_dns_request() -> Request {
    let mut message = Message::new(1, MessageType::Query, OpCode::Query);
    let mut query = Query::new();
    query.set_name(Name::from_str("example.com.").unwrap());
    query.set_query_type(RecordType::A);
    message.add_query(query);
    let bytes = message.to_vec().unwrap();
    Request::from_bytes(
        bytes,
        SocketAddr::from(([127, 0, 0, 1], 5353)),
        Protocol::Udp,
    )
    .unwrap()
}
