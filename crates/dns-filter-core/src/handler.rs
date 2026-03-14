use async_trait::async_trait;
use dns_filter_plugin::authority_chain::AuthorityChain;
use dns_filter_plugin::{Middleware, MiddlewareAction};
use hickory_proto::op::{Header, MessageType, OpCode, ResponseCode};
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{LookupControlFlow, MessageResponseBuilder};
use std::sync::Arc;
use std::time::Instant;

pub struct DnsFilterHandler {
    middlewares: Vec<Arc<dyn Middleware>>,
    authority_chain: Arc<AuthorityChain>,
}

impl DnsFilterHandler {
    pub fn new(
        middlewares: Vec<Arc<dyn Middleware>>,
        authority_chain: Arc<AuthorityChain>,
    ) -> Self {
        Self {
            middlewares,
            authority_chain,
        }
    }
}

#[async_trait]
impl RequestHandler for DnsFilterHandler {
    async fn handle_request<R: ResponseHandler, T: hickory_server::net::runtime::Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let start = Instant::now();

        // Run before_request on all middlewares
        for mw in &self.middlewares {
            if let MiddlewareAction::ShortCircuit = mw.before_request(request).await {
                let duration_ms = start.elapsed().as_millis() as u64;
                for mw in &self.middlewares {
                    mw.after_request(request, duration_ms).await;
                }
                return send_error(request, &mut response_handle, ResponseCode::Refused).await;
            }
        }

        // Extract query info
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(_) => {
                return send_error(request, &mut response_handle, ResponseCode::FormErr).await;
            }
        };

        // Resolve through authority chain
        let query_name = request_info.query.name().to_owned();
        let query_type = request_info.query.query_type();
        let result = self
            .authority_chain
            .resolve(&query_name.into(), query_type, Some(&request_info))
            .await;

        let response_info = match result {
            LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
                let builder = MessageResponseBuilder::from_message_request(request);
                let mut header = Header::response_from_request(request.header());
                header.set_message_type(MessageType::Response);
                header.set_op_code(OpCode::Query);
                header.set_response_code(ResponseCode::NoError);
                header.set_recursion_available(true);

                let records: Vec<_> = lookup.iter().cloned().collect();
                let response = builder.build(header, records.iter(), &[], &[], &[]);

                match response_handle.send_response(response).await {
                    Ok(info) => info,
                    Err(_) => {
                        send_error(request, &mut response_handle, ResponseCode::ServFail).await
                    }
                }
            }
            LookupControlFlow::Continue(Err(_)) | LookupControlFlow::Break(Err(_)) => {
                send_error(request, &mut response_handle, ResponseCode::ServFail).await
            }
            LookupControlFlow::Skip => {
                send_error(request, &mut response_handle, ResponseCode::NXDomain).await
            }
        };

        // Run after_request on all middlewares
        let duration_ms = start.elapsed().as_millis() as u64;
        for mw in &self.middlewares {
            mw.after_request(request, duration_ms).await;
        }

        response_info
    }
}

async fn send_error<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    response_code: ResponseCode,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let response = builder.error_msg(request.header(), response_code);
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(_) => {
            let mut header = Header::response_from_request(request.header());
            header.set_response_code(ResponseCode::ServFail);
            header.into()
        }
    }
}
