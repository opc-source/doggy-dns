use async_trait::async_trait;
use dns_filter_plugin::authority_chain::AuthorityChain;
use dns_filter_plugin::{Middleware, MiddlewareAction};
use hickory_proto::op::{Header, MessageType, OpCode, ResponseCode};
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{LookupControlFlow, MessageResponseBuilder};
use std::sync::Arc;
use tokio::time::Instant;

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
                tracing::warn!(
                    duration_ms = duration_ms,
                    response_code = ?ResponseCode::Refused,
                    "request short-circuited by middleware"
                );
                return send_error(request, &mut response_handle, ResponseCode::Refused).await;
            }
        }

        // Extract query info
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse request info");
                return send_error(request, &mut response_handle, ResponseCode::FormErr).await;
            }
        };

        // Resolve through authority chain
        let query_name = request_info.query.name().to_owned();
        let query_type = request_info.query.query_type();
        let query_display = query_name.to_string();
        let resolve_result = self
            .authority_chain
            .resolve(&query_name.into(), query_type, Some(&request_info))
            .await;

        let handler_name = resolve_result.handler_name.as_deref().unwrap_or("none");

        let response_info = match resolve_result.outcome {
            LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
                let builder = MessageResponseBuilder::from_message_request(request);
                let mut header = Header::response_from_request(request.header());
                header.set_message_type(MessageType::Response);
                header.set_op_code(OpCode::Query);
                header.set_response_code(ResponseCode::NoError);
                header.set_recursion_available(true);

                let records: Vec<_> = lookup.iter().cloned().collect();
                let answer_count = records.len();
                let response = builder.build(header, records.iter(), &[], &[], &[]);

                match response_handle.send_response(response).await {
                    Ok(info) => {
                        tracing::info!(
                            query = %query_display,
                            qtype = ?query_type,
                            authority = handler_name,
                            answers = answer_count,
                            duration_ms = start.elapsed().as_millis() as u64,
                            response_code = ?ResponseCode::NoError,
                            "query resolved"
                        );
                        info
                    }
                    Err(e) => {
                        tracing::error!(
                            query = %query_display,
                            qtype = ?query_type,
                            authority = handler_name,
                            error = %e,
                            "failed to send response"
                        );
                        send_error(request, &mut response_handle, ResponseCode::ServFail).await
                    }
                }
            }
            LookupControlFlow::Continue(Err(e)) | LookupControlFlow::Break(Err(e)) => {
                tracing::warn!(
                    query = %query_display,
                    qtype = ?query_type,
                    authority = handler_name,
                    error = %e,
                    response_code = ?ResponseCode::ServFail,
                    "authority returned error"
                );
                send_error(request, &mut response_handle, ResponseCode::ServFail).await
            }
            LookupControlFlow::Skip => {
                tracing::info!(
                    query = %query_display,
                    qtype = ?query_type,
                    response_code = ?ResponseCode::NXDomain,
                    "no authority handled query"
                );
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
        Err(e) => {
            tracing::error!(
                response_code = ?response_code,
                error = %e,
                "failed to send error response"
            );
            let mut header = Header::response_from_request(request.header());
            header.set_response_code(ResponseCode::ServFail);
            header.into()
        }
    }
}
