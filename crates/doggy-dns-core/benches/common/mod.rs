#![allow(dead_code)]
#![allow(clippy::disallowed_methods)]

//! Shared scaffolding for the handler benches: a request to feed the pipeline
//! and a response sink that keeps the answer bytes out of the measurement.

use async_trait::async_trait;
use hickory_proto::op::{Header, HeaderCounts, Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, Record, RecordType};
use hickory_server::net::NetError;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::MessageResponse;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

#[derive(Clone)]
pub struct NoOpResponseHandler;

#[async_trait]
impl ResponseHandler for NoOpResponseHandler {
    async fn send_response<'a>(
        &mut self,
        response: MessageResponse<
            '_,
            'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
        >,
    ) -> Result<ResponseInfo, NetError> {
        let metadata = *response.metadata();
        Ok(ResponseInfo::from(Header {
            metadata,
            counts: HeaderCounts::default(),
        }))
    }
}

pub fn make_request(domain: &str) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(RecordType::A);
    message.add_query(query);
    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}
