use async_trait::async_trait;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::{Header, HeaderCounts, MessageType, Metadata, OpCode, ResponseCode};
use hickory_proto::rr::Record;
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupError, LookupRecords, MessageResponseBuilder,
};
use std::sync::Arc;
use tokio::time::Instant;
use tracing::Instrument;

pub struct DoggyDnsHandler {
    middlewares: Vec<Arc<dyn Middleware>>,
    authority_chain: Arc<AuthorityChain>,
    xdp_enabled: bool,
}

impl DoggyDnsHandler {
    pub fn new(
        middlewares: Vec<Arc<dyn Middleware>>,
        authority_chain: Arc<AuthorityChain>,
        xdp_enabled: bool,
    ) -> Self {
        Self {
            middlewares,
            authority_chain,
            xdp_enabled,
        }
    }

    async fn finish(
        &self,
        request: &Request,
        start: Instant,
        span: &tracing::Span,
        outcome: &RequestOutcome,
    ) {
        let duration_ms = start.elapsed().as_millis() as u64;
        record_outcome(span, outcome, duration_ms);
        for middleware in &self.middlewares {
            middleware
                .after_request(request, duration_ms, outcome)
                .await;
        }
    }
}

#[async_trait]
impl RequestHandler for DoggyDnsHandler {
    async fn handle_request<R: ResponseHandler, T: hickory_server::net::runtime::Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let start = Instant::now();
        let request_id = request.metadata.id;

        let span = tracing::info_span!(
            "dns.request",
            otel.kind = "server",
            request_id = %request_id,
            "dns.question.name" = tracing::field::Empty,
            "dns.question.type" = tracing::field::Empty,
            "network.protocol.name" = tracing::field::Empty,
            "client.address" = tracing::field::Empty,
            "client.port" = tracing::field::Empty,
            "dns.response.code" = tracing::field::Empty,
            "dns.authority" = tracing::field::Empty,
            "dns.answers" = tracing::field::Empty,
            "dns.duration_ms" = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let span_for_body = span.clone();

        async move {
            // Run before_request on all middlewares
            for middleware in &self.middlewares {
                if let MiddlewareAction::ShortCircuit = middleware.before_request(request).await {
                    let outcome = RequestOutcome::error(ResponseCode::Refused);
                    self.finish(request, start, &span_for_body, &outcome).await;
                    tracing::warn!(
                        request_id = request_id,
                        duration_ms = start.elapsed().as_millis() as u64,
                        response_code = ?ResponseCode::Refused,
                        "request short-circuited by middleware"
                    );
                    return send_error(
                        request,
                        &mut response_handle,
                        ResponseCode::Refused,
                        false,
                        None,
                    )
                    .await;
                }
            }

            // Extract query info
            let request_info = match request.request_info() {
                Ok(info) => info,
                Err(e) => {
                    let outcome = RequestOutcome::error(ResponseCode::FormErr);
                    self.finish(request, start, &span_for_body, &outcome).await;
                    tracing::warn!(
                        request_id = request_id,
                        error = %e,
                        "failed to parse request info"
                    );
                    return send_error(
                        request,
                        &mut response_handle,
                        ResponseCode::FormErr,
                        false,
                        None,
                    )
                    .await;
                }
            };

            span_for_body.record(
                "dns.question.name",
                tracing::field::display(request_info.query.name()),
            );
            span_for_body.record(
                "dns.question.type",
                tracing::field::display(request_info.query.query_type()),
            );
            span_for_body.record(
                "network.protocol.name",
                tracing::field::display(
                    format!("{:?}", request_info.protocol).to_ascii_lowercase(),
                ),
            );
            span_for_body.record(
                "client.address",
                tracing::field::display(request_info.src.ip()),
            );
            span_for_body.record("client.port", request_info.src.port());

            // Resolve through authority chain. `query.name()` is the `LowerName`
            // the server already lowercased once at parse time, so it goes to the
            // chain by reference; the log string is the only owned copy.
            let query_type = request_info.query.query_type();
            let query_display = request_info.query.name().to_string();
            let resolve_span = tracing::info_span!(
                "dns.resolve",
                "dns.question.name" = %query_display,
                "dns.question.type" = tracing::field::display(query_type),
                "dns.authority" = tracing::field::Empty,
                "dns.answers" = tracing::field::Empty,
            );
            let resolve_result = self
                .authority_chain
                .resolve(request_info.query.name(), query_type, Some(&request_info))
                .instrument(resolve_span.clone())
                .await;

            let authoritative = resolve_result.authoritative;
            let soa = resolve_result.soa;
            let handler_name = resolve_result.handler_name.as_deref().unwrap_or("none");
            resolve_span.record(
                "dns.authority",
                tracing::field::display(handler_name.to_string()),
            );

            let (response_info, outcome) = match resolve_result.outcome {
                LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
                    let records = take_answer_records(lookup);
                    let answer_count = records.len();

                    let mut metadata = Metadata::response_from_request(&request.metadata);
                    metadata.message_type = MessageType::Response;
                    metadata.op_code = OpCode::Query;
                    metadata.response_code = ResponseCode::NoError;
                    // AA marks data the server owns; RA only when the answer
                    // came from a recursive backend (forward/native).
                    metadata.authoritative = authoritative;
                    metadata.recursion_available = !authoritative;

                    let resp_bytes = if self.xdp_enabled {
                        serialize_for_xdp(request, |probe| {
                            let b = MessageResponseBuilder::from_message_request(request);
                            b.build(metadata, records.iter(), &[], &[], &[])
                                .destructive_emit(probe)
                                .is_ok()
                        })
                    } else {
                        None
                    };

                    let min_ttl = if self.xdp_enabled {
                        min_ttl_from_records(&records)
                    } else {
                        None
                    };

                    let send_builder = MessageResponseBuilder::from_message_request(request);
                    let response = send_builder.build(metadata, records.iter(), &[], &[], &[]);

                    match response_handle.send_response(response).await {
                        Ok(info) => {
                            tracing::info!(
                                request_id = request_id,
                                query = %query_display,
                                qtype = ?query_type,
                                authority = handler_name,
                                answers = answer_count,
                                duration_ms = start.elapsed().as_millis() as u64,
                                response_code = ?ResponseCode::NoError,
                                "query resolved"
                            );
                            (
                                info,
                                RequestOutcome::success(
                                    handler_name.to_string(),
                                    answer_count,
                                    resp_bytes,
                                    min_ttl,
                                ),
                            )
                        }
                        Err(e) => {
                            tracing::error!(
                                request_id = request_id,
                                query = %query_display,
                                qtype = ?query_type,
                                authority = handler_name,
                                error = %e,
                                "failed to send response"
                            );
                            (
                                send_error(
                                    request,
                                    &mut response_handle,
                                    ResponseCode::ServFail,
                                    false,
                                    None,
                                )
                                .await,
                                RequestOutcome::authority_error(
                                    ResponseCode::ServFail,
                                    handler_name.to_string(),
                                ),
                            )
                        }
                    }
                }
                LookupControlFlow::Continue(Err(e)) | LookupControlFlow::Break(Err(e)) => {
                    let response_code = response_code_for_lookup_error(&e);
                    let negative_cacheable = matches!(
                        response_code,
                        ResponseCode::NoError | ResponseCode::NXDomain
                    );
                    let resp_bytes = if self.xdp_enabled && negative_cacheable {
                        serialize_for_xdp(request, |probe| {
                            let b = MessageResponseBuilder::from_message_request(request);
                            b.build(
                                error_metadata(request, response_code, authoritative),
                                std::iter::empty::<&Record>(),
                                &[],
                                soa.iter(),
                                &[],
                            )
                            .destructive_emit(probe)
                            .is_ok()
                        })
                    } else {
                        None
                    };

                    if response_code == ResponseCode::ServFail {
                        tracing::warn!(
                            request_id = request_id,
                            query = %query_display,
                            qtype = ?query_type,
                            authority = handler_name,
                            error = %e,
                            response_code = ?response_code,
                            "authority returned error"
                        );
                    } else {
                        tracing::debug!(
                            request_id = request_id,
                            query = %query_display,
                            qtype = ?query_type,
                            authority = handler_name,
                            response_code = ?response_code,
                            "authority returned negative response"
                        );
                    }

                    let mut outcome =
                        RequestOutcome::authority_error(response_code, handler_name.to_string());
                    outcome.response_bytes = resp_bytes;
                    (
                        send_error(
                            request,
                            &mut response_handle,
                            response_code,
                            authoritative,
                            soa.as_ref(),
                        )
                        .await,
                        outcome,
                    )
                }
                LookupControlFlow::Skip => {
                    let resp_bytes = if self.xdp_enabled {
                        serialize_for_xdp(request, |probe| {
                            let b = MessageResponseBuilder::from_message_request(request);
                            b.build(
                                error_metadata(request, ResponseCode::NXDomain, false),
                                std::iter::empty::<&Record>(),
                                &[],
                                std::iter::empty::<&Record>(),
                                &[],
                            )
                            .destructive_emit(probe)
                            .is_ok()
                        })
                    } else {
                        None
                    };

                    tracing::info!(
                        request_id = request_id,
                        query = %query_display,
                        qtype = ?query_type,
                        response_code = ?ResponseCode::NXDomain,
                        "no authority handled query"
                    );
                    (
                        send_error(
                            request,
                            &mut response_handle,
                            ResponseCode::NXDomain,
                            false,
                            None,
                        )
                        .await,
                        RequestOutcome::nxdomain(resp_bytes),
                    )
                }
            };

            resolve_span.record("dns.answers", tracing::field::display(outcome.answers));
            self.finish(request, start, &span_for_body, &outcome).await;

            response_info
        }
        .instrument(span)
        .await
    }
}

/// Map an authority lookup error to the DNS response code it represents.
fn response_code_for_lookup_error(error: &LookupError) -> ResponseCode {
    match error {
        LookupError::NameExists => ResponseCode::NoError,
        LookupError::ResponseCode(code) => *code,
        LookupError::NetError(e) if e.is_nx_domain() => ResponseCode::NXDomain,
        LookupError::NetError(e) if e.is_no_records_found() => ResponseCode::NoError,
        _ => ResponseCode::ServFail,
    }
}

fn record_outcome(span: &tracing::Span, outcome: &RequestOutcome, duration_ms: u64) {
    let code = format!("{:?}", outcome.response_code).to_ascii_uppercase();
    span.record("dns.response.code", tracing::field::display(&code));
    span.record("dns.duration_ms", duration_ms);
    span.record(
        "dns.authority",
        tracing::field::display(outcome.authority.as_deref().unwrap_or("none")),
    );
    span.record("dns.answers", tracing::field::display(outcome.answers));

    let status = if matches!(
        outcome.response_code,
        ResponseCode::NoError | ResponseCode::NXDomain
    ) {
        "OK"
    } else {
        "ERROR"
    };
    span.record("otel.status_code", status);
}

/// Maximum DNS response size (in bytes) that can be stored in the XDP cache.
/// Must match `CacheValue::resp_data` length in doggy-dns-xdp and the eBPF program.
const XDP_CACHE_MAX_SIZE: usize = 512;

/// Serialize a DNS response to wire format for the XDP cache.
///
/// The caller supplies a closure that builds and emits the response into the
/// provided [`BinEncoder`]. The response is serialized with the client's
/// advertised max size, then admitted to the cache only when the actual wire
/// length fits [`XDP_CACHE_MAX_SIZE`] and the response is not truncated.
/// Returns `None` in all other cases (those responses fall through to userspace).
fn serialize_for_xdp(
    request: &Request,
    emit_fn: impl FnOnce(&mut BinEncoder<'_>) -> bool,
) -> Option<Vec<u8>> {
    let max_size = max_message_size(request.protocol(), request.edns.as_ref());

    let mut bytes = Vec::with_capacity(XDP_CACHE_MAX_SIZE);
    let mut encoder = BinEncoder::new(&mut bytes);
    encoder.set_max_size(max_size);
    if !emit_fn(&mut encoder) {
        tracing::warn!(
            request_id = request.metadata.id,
            "failed to serialize response for XDP cache"
        );
        return None;
    }

    let skip_reason = if bytes.len() > XDP_CACHE_MAX_SIZE {
        Some("response exceeds XDP cache capacity")
    } else if bytes.get(2).is_some_and(|flags| flags & 0x02 != 0) {
        // TC bit: a partial answer must never be served from cache.
        Some("response is truncated")
    } else {
        None
    };
    if let Some(reason) = skip_reason {
        if let Ok(info) = request.request_info() {
            tracing::debug!(
                request_id = request.metadata.id,
                query = %info.query.name(),
                qtype = %info.query.query_type(),
                protocol = ?info.protocol,
                client = %info.src,
                bytes = bytes.len(),
                limit = XDP_CACHE_MAX_SIZE,
                reason,
                "skipping XDP cache"
            );
        } else {
            tracing::debug!(
                request_id = request.metadata.id,
                bytes = bytes.len(),
                limit = XDP_CACHE_MAX_SIZE,
                reason,
                "skipping XDP cache"
            );
        }
        return None;
    }

    Some(bytes)
}

/// Move the answer records out of a zone lookup instead of cloning them.
///
/// The authorities we own build answers as `LookupRecords::Section`, an owned
/// `Vec`, so handing it over is a free transfer of records that used to cost
/// two allocations per answer through `iter().cloned()`. `forward` and `native`
/// return `AuthLookup::Resolved`, whose records live behind an `Arc`, so those
/// keep the clone path. Order is preserved: `Section` iterates forward, the
/// same direction the clone walked, and `AuthLookup::iter` already ignored
/// `additionals`, so the `..` binding drops nothing the response carried.
fn take_answer_records(lookup: AuthLookup) -> Vec<Record> {
    match lookup {
        AuthLookup::Records {
            answers: LookupRecords::Section(records),
            ..
        } => records,
        lookup => lookup.iter().cloned().collect(),
    }
}

/// Extract the minimum TTL from a slice of DNS records.
fn min_ttl_from_records(records: &[hickory_proto::rr::Record]) -> Option<u32> {
    records.iter().map(|r| r.ttl).min()
}

/// Compute the max message size for the BinEncoder, matching hickory-server's internal logic.
fn max_message_size(protocol: Protocol, edns: Option<&hickory_proto::op::Edns>) -> u16 {
    match protocol {
        Protocol::Udp => match edns {
            Some(edns) => edns.max_payload(),
            None => 512,
        },
        _ => u16::MAX,
    }
}

/// Build response metadata with the AA/RA flags that match the answering
/// authority: authoritative zones set AA and clear RA, recursive backends do
/// the opposite.
fn error_metadata(request: &Request, response_code: ResponseCode, authoritative: bool) -> Metadata {
    let mut metadata = Metadata::response_from_request(&request.metadata);
    metadata.message_type = MessageType::Response;
    metadata.op_code = OpCode::Query;
    metadata.response_code = response_code;
    metadata.authoritative = authoritative;
    metadata.recursion_available = !authoritative;
    metadata
}

async fn send_error<R: ResponseHandler>(
    request: &Request,
    response_handle: &mut R,
    response_code: ResponseCode,
    authoritative: bool,
    soa: Option<&Record>,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let response = builder.build(
        error_metadata(request, response_code, authoritative),
        std::iter::empty::<&Record>(),
        &[],
        soa,
        &[],
    );
    match response_handle.send_response(response).await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!(
                response_code = ?response_code,
                error = %e,
                "failed to send error response"
            );
            let mut metadata = error_metadata(request, ResponseCode::ServFail, false);
            metadata.response_code = ResponseCode::ServFail;
            Header {
                metadata,
                counts: HeaderCounts::default(),
            }
            .into()
        }
    }
}
