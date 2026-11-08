use async_trait::async_trait;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::{Header, HeaderCounts, MessageType, Metadata, OpCode, ResponseCode};
use hickory_proto::rr::{LowerName, Record, RecordType};
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, RequestInfo, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupError, LookupRecords, MessageResponseBuilder,
};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::Instant;
use tracing::Instrument;

/// Cycles the answer records of a multi-record response, one step per rotated
/// response.
///
/// A DNS client that takes the first record — which is what most resolvers and
/// every `getaddrinfo` caller effectively do — otherwise keeps landing on the
/// same instance, because the authority emits them in registration order.
/// The cursor is one counter for the whole handler, not one per name: every
/// rotated response moves it by one, whereas a per-name cursor would need a
/// table that grows with the names seen and an eviction policy to go with it.
/// Traffic on other names only makes a given name's step jump further ahead,
/// which spreads no worse.
/// Rotation moves records inside the already-owned `Vec`, so it costs no
/// allocation.
#[derive(Debug)]
pub struct AnswerRotation {
    enabled: bool,
    cursor: AtomicUsize,
}

impl AnswerRotation {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            cursor: AtomicUsize::new(0),
        }
    }

    /// Advances the cursor and rotates `records` so its first entry walks the
    /// instance list. Single-record and empty answers are left alone, which is
    /// the common case: one instance, or a negative answer.
    pub fn apply(&self, records: &mut [Record]) {
        let len = records.len();
        if !self.enabled || len < 2 {
            return;
        }
        let offset = self.cursor.fetch_add(1, Ordering::Relaxed) % len;
        records.rotate_left(offset);
    }
}

/// The per-query facts every response path logs and reports, fixed once the
/// authority has answered.
struct QueryContext<'a> {
    request_id: u16,
    query: &'a LowerName,
    qtype: RecordType,
    /// The handler that answered, or `None` when the chain skipped everything.
    authority: Option<&'a Arc<str>>,
    authoritative: bool,
    start: Instant,
}

impl QueryContext<'_> {
    /// The name the logs print; "none" when no handler claimed the query.
    fn authority_name(&self) -> &str {
        self.authority.map_or("none", |name| &**name)
    }

    /// The same name for [`RequestOutcome`], which keeps its own handle.
    fn authority_owned(&self) -> Arc<str> {
        self.authority.cloned().unwrap_or_else(|| Arc::from("none"))
    }
}

pub struct DoggyDnsHandler {
    middlewares: Vec<Arc<dyn Middleware>>,
    authority_chain: Arc<AuthorityChain>,
    xdp_enabled: bool,
    rotation: AnswerRotation,
}

impl DoggyDnsHandler {
    pub fn new(
        middlewares: Vec<Arc<dyn Middleware>>,
        authority_chain: Arc<AuthorityChain>,
        xdp_enabled: bool,
        rotation: AnswerRotation,
    ) -> Self {
        Self {
            middlewares,
            authority_chain,
            xdp_enabled,
            rotation,
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

    /// Send a successful lookup. Rotation happens before anything reads the
    /// order, so the response, the XDP cache payload and the answer count all
    /// see one sequence.
    async fn send_ok<R: ResponseHandler>(
        &self,
        request: &Request,
        response_handle: &mut R,
        lookup: AuthLookup,
        ctx: &QueryContext<'_>,
    ) -> (ResponseInfo, RequestOutcome) {
        let mut records = take_answer_records(lookup);
        self.rotation.apply(&mut records);
        let answer_count = records.len();

        // AA marks data the server owns; RA only when the answer
        // came from a recursive backend (forward/native).
        let metadata = response_metadata(request, ResponseCode::NoError, ctx.authoritative);

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
                    request_id = ctx.request_id,
                    query = %ctx.query,
                    qtype = ?ctx.qtype,
                    authority = %ctx.authority_name(),
                    answers = answer_count,
                    duration_ms = ctx.start.elapsed().as_millis() as u64,
                    response_code = ?ResponseCode::NoError,
                    "query resolved"
                );
                (
                    info,
                    RequestOutcome::success(
                        ctx.authority_owned(),
                        answer_count,
                        resp_bytes,
                        min_ttl,
                    ),
                )
            }
            Err(e) => {
                tracing::error!(
                    request_id = ctx.request_id,
                    query = %ctx.query,
                    qtype = ?ctx.qtype,
                    authority = %ctx.authority_name(),
                    error = %e,
                    "failed to send response"
                );
                (
                    send_error(
                        request,
                        response_handle,
                        ResponseCode::ServFail,
                        false,
                        None,
                    )
                    .await,
                    RequestOutcome::authority_error(ResponseCode::ServFail, ctx.authority_owned()),
                )
            }
        }
    }

    /// Send the answer an authority could not produce. Only a negative one is
    /// worth caching, and only a SERVFAIL is worth a warning.
    async fn send_lookup_error<R: ResponseHandler>(
        &self,
        request: &Request,
        response_handle: &mut R,
        error: &LookupError,
        soa: Option<&Record>,
        ctx: &QueryContext<'_>,
    ) -> (ResponseInfo, RequestOutcome) {
        let response_code = response_code_for_lookup_error(error);
        let negative_cacheable = matches!(
            response_code,
            ResponseCode::NoError | ResponseCode::NXDomain
        );
        let resp_bytes = if self.xdp_enabled && negative_cacheable {
            xdp_error_payload(request, response_code, ctx.authoritative, soa)
        } else {
            None
        };

        if response_code == ResponseCode::ServFail {
            tracing::warn!(
                request_id = ctx.request_id,
                query = %ctx.query,
                qtype = ?ctx.qtype,
                authority = %ctx.authority_name(),
                error = %error,
                response_code = ?response_code,
                "authority returned error"
            );
        } else {
            tracing::debug!(
                request_id = ctx.request_id,
                query = %ctx.query,
                qtype = ?ctx.qtype,
                authority = %ctx.authority_name(),
                response_code = ?response_code,
                "authority returned negative response"
            );
        }

        let mut outcome = RequestOutcome::authority_error(response_code, ctx.authority_owned());
        outcome.response_bytes = resp_bytes;
        (
            send_error(
                request,
                response_handle,
                response_code,
                ctx.authoritative,
                soa,
            )
            .await,
            outcome,
        )
    }

    /// Send the `NXDOMAIN` for a query that no authority in the chain claimed.
    /// There is no answering authority to name and nothing recursive about the
    /// refusal, so both the log and the response leave those out.
    async fn send_unhandled<R: ResponseHandler>(
        &self,
        request: &Request,
        response_handle: &mut R,
        ctx: &QueryContext<'_>,
    ) -> (ResponseInfo, RequestOutcome) {
        let resp_bytes = if self.xdp_enabled {
            xdp_error_payload(request, ResponseCode::NXDomain, false, None)
        } else {
            None
        };

        tracing::info!(
            request_id = ctx.request_id,
            query = %ctx.query,
            qtype = ?ctx.qtype,
            response_code = ?ResponseCode::NXDomain,
            "no authority handled query"
        );
        (
            send_error(
                request,
                response_handle,
                ResponseCode::NXDomain,
                false,
                None,
            )
            .await,
            RequestOutcome::nxdomain(resp_bytes),
        )
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

            record_query_fields(&span_for_body, &request_info);

            // Resolve through authority chain. `query.name()` is the `LowerName`
            // the server already lowercased once at parse time, so it goes to the
            // chain by reference and the spans print it by reference too.
            let query_type = request_info.query.query_type();
            let query_name = request_info.query.name();
            let resolve_span = tracing::info_span!(
                "dns.resolve",
                "dns.question.name" = %query_name,
                "dns.question.type" = tracing::field::display(query_type),
                "dns.authority" = tracing::field::Empty,
                "dns.answers" = tracing::field::Empty,
            );
            let resolve_result = self
                .authority_chain
                .resolve(query_name, query_type, Some(&request_info))
                .instrument(resolve_span.clone())
                .await;

            let authoritative = resolve_result.authoritative;
            let soa = resolve_result.soa;
            resolve_span.record(
                "dns.authority",
                tracing::field::display(resolve_result.handler_name.as_deref().unwrap_or("none")),
            );

            // What every response path logs and reports, fixed once the
            // authority has answered.
            let ctx = QueryContext {
                request_id,
                query: query_name,
                qtype: query_type,
                authority: resolve_result.handler_name.as_ref(),
                authoritative,
                start,
            };

            let (response_info, outcome) = match resolve_result.outcome {
                LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
                    self.send_ok(request, &mut response_handle, lookup, &ctx)
                        .await
                }
                LookupControlFlow::Continue(Err(e)) | LookupControlFlow::Break(Err(e)) => {
                    self.send_lookup_error(request, &mut response_handle, &e, soa.as_deref(), &ctx)
                        .await
                }
                LookupControlFlow::Skip => {
                    self.send_unhandled(request, &mut response_handle, &ctx)
                        .await
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

/// The lowercase transport name `network.protocol.name` carries.
///
/// UDP and TCP — everything a plain DNS listener sees — answer with a static
/// string; the feature-gated transports keep the derived spelling.
fn protocol_name(protocol: Protocol) -> Cow<'static, str> {
    match protocol {
        Protocol::Udp => Cow::Borrowed("udp"),
        Protocol::Tcp => Cow::Borrowed("tcp"),
        _ => Cow::Owned(format!("{protocol:?}").to_ascii_lowercase()),
    }
}

/// The uppercase mnemonic `dns.response.code` carries.
///
/// The codes a query actually ends in are static, so the common paths format
/// nothing; anything rarer keeps the derived `Debug` spelling.
fn response_code_name(code: ResponseCode) -> Cow<'static, str> {
    let name = match code {
        ResponseCode::NoError => "NOERROR",
        ResponseCode::FormErr => "FORMERR",
        ResponseCode::ServFail => "SERVFAIL",
        ResponseCode::NXDomain => "NXDOMAIN",
        ResponseCode::NotImp => "NOTIMP",
        ResponseCode::Refused => "REFUSED",
        _ => return Cow::Owned(format!("{code:?}").to_ascii_uppercase()),
    };
    Cow::Borrowed(name)
}

/// Attach the query, transport and client to the request span. These are the
/// attributes every response path shares, so they are recorded once here,
/// before the authority is asked.
fn record_query_fields(span: &tracing::Span, info: &RequestInfo<'_>) {
    span.record(
        "dns.question.name",
        tracing::field::display(info.query.name()),
    );
    span.record(
        "dns.question.type",
        tracing::field::display(info.query.query_type()),
    );
    span.record(
        "network.protocol.name",
        tracing::field::display(protocol_name(info.protocol)),
    );
    span.record("client.address", tracing::field::display(info.src.ip()));
    span.record("client.port", info.src.port());
}

fn record_outcome(span: &tracing::Span, outcome: &RequestOutcome, duration_ms: u64) {
    let code = response_code_name(outcome.response_code);
    span.record("dns.response.code", tracing::field::display(code));
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

/// The XDP cache payload for a response that carries no answer records: the
/// negative and all-skipped paths share this shape, and only the SOA in the
/// authority section tells them apart.
fn xdp_error_payload(
    request: &Request,
    response_code: ResponseCode,
    authoritative: bool,
    soa: Option<&Record>,
) -> Option<Vec<u8>> {
    serialize_for_xdp(request, |probe| {
        MessageResponseBuilder::from_message_request(request)
            .build(
                response_metadata(request, response_code, authoritative),
                std::iter::empty::<&Record>(),
                &[],
                soa,
                &[],
            )
            .destructive_emit(probe)
            .is_ok()
    })
}

/// Move the answer records out of a zone lookup instead of cloning them.
///
/// The authorities we own build answers as `LookupRecords::Section`, an owned
/// `Vec`, so handing it over is a free transfer of records that used to be
/// rebuilt by `iter().cloned()`: one `Vec` allocation plus a `Name` clone per
/// record, and that clone allocates as soon as the name outgrows `Name`'s
/// inline buffer. `forward` and `native` return `AuthLookup::Resolved`, whose
/// records live behind an `Arc`, so those keep the clone path. Order is
/// preserved: `Section` iterates forward, the same direction the clone walked,
/// and `AuthLookup::iter` already ignored `additionals`, so the `..` binding
/// drops nothing the response carried.
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
fn response_metadata(
    request: &Request,
    response_code: ResponseCode,
    authoritative: bool,
) -> Metadata {
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
        response_metadata(request, response_code, authoritative),
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
            Header {
                metadata: response_metadata(request, ResponseCode::ServFail, false),
                counts: HeaderCounts::default(),
            }
            .into()
        }
    }
}
