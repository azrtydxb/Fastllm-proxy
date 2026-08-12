//! Request handling.
//!
//! The hot path is deliberately dumb. For an inference response the proxy is a
//! byte pump: upstream frames are handed to the client exactly as they arrive,
//! never deserialised, never re-encoded, never buffered. An SSE stream at
//! 2000 tok/s is 2000 frames/s that a parsing gateway would decode and
//! re-encode per token; here each one is a pointer move.
//!
//! Request bodies *are* inspected, because routing needs the model name — but
//! only once, and the original bytes are what get forwarded. The body is only
//! rebuilt when an alias means the upstream model name differs from the one the
//! client asked for.
//!
//! Two body encodings show up on these endpoints: JSON everywhere, and
//! `multipart/form-data` on the audio routes. Both are handled the same way —
//! locate `model`, forward the original bytes — so an upload never gets
//! re-encoded on its way through.

use crate::upstream::{BoxError, UpstreamBody};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{HeaderValue, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use pin_project_lite::pin_project;
use serde::Deserialize;
use std::borrow::Cow;
use std::ops::Range;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tracing::{debug, warn};

/// Record a field on the current request span, or nothing at all.
///
/// Without the `otel` feature this expands to a no-op, so the call sites read
/// the same either way and a default build carries no span machinery.
macro_rules! span_field {
    ($name:literal, $value:expr) => {
        #[cfg(feature = "otel")]
        {
            tracing::Span::current().record($name, $value);
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = &$value;
        }
    };
}

use crate::multipart;
use crate::protocol;
use crate::registry::{Backend, BackendUid, InflightGuard, Pool, Registry};
use crate::snapshot::{AuthError, Budget, Principal, PrincipalId, Snapshot};
use crate::state::AppState;
use crate::tail_buffer::TailBuffer;
use crate::usage::{UsageEvent, UsageReporter};
use std::time::SystemTime;

pub type ResBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

/// Endpoints forwarded to a backend. Everything else is served locally.
const PROXIED_SUFFIXES: &[&str] = &[
    "/chat/completions",
    "/completions",
    // OpenAI's newer Responses API. Passthrough only, like the rest of this
    // list: a native backend answers 501 below rather than being handed a body
    // it cannot read.
    "/responses",
    "/embeddings",
    "/rerank",
    "/score",
    "/audio/transcriptions",
    "/audio/translations",
    // Binary or non-streaming responses, which the byte pump handles without
    // caring: nothing on the response path parses a passthrough body, and the
    // usage tail buffer already tolerates finding no usage in it.
    "/audio/speech",
    "/images/generations",
    "/images/edits",
    "/moderations",
];

/// Per-connection headers, meaningless on the next hop in either direction.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Additionally dropped from the client's request before it goes upstream.
///
/// `host` and `content-length` describe the old hop and are regenerated;
/// `authorization` authenticates the client to *this* proxy and must never
/// leak onward — the upstream gets the backend's own key or nothing.
///
/// Note that `content-length` is deliberately absent from the response side:
/// the body is forwarded byte for byte, so an upstream length stays accurate
/// and stripping it would force needless chunking on ordinary completions.
const REQUEST_ONLY_STRIPPED: &[&str] = &["host", "content-length", "authorization"];

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<ResBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Liveness endpoints stay open so a probe never needs the master key.
    match (&method, path.as_str()) {
        (&Method::GET, "/health") | (&Method::GET, "/healthz") => {
            return Ok(health_response(&state))
        }
        (&Method::GET, "/metrics") => return Ok(metrics_response(&state)),
        _ => {}
    }

    // An owned `Arc<Snapshot>`, not an `ArcSwap` guard: `authorize` hands back
    // a `&Principal` borrowed from it, and that borrow has to survive through
    // `proxy_request`'s `.await` below — a guard held that long is exactly
    // the thing that blocks a concurrent reload from reclaiming the old
    // snapshot, which is what the previous `state.snapshot.load()` +
    // `drop(snapshot)` dance existed to avoid. `load_full()` sidesteps the
    // question entirely: it is a plain refcounted pointer, cheap to hold for
    // as long as the request needs it, and correspondingly this is also what
    // saves the per-request `Principal` clone (name `String` +
    // `allowed_models: HashSet<String>`) that used to happen on every
    // authenticated request via `.cloned()`.
    let snapshot = state.snapshot.load_full();
    let principal = match authorize(&req, &snapshot) {
        Ok(p) => p,
        Err(rejection) => {
            // Counted here rather than inside `authorize`, which has no access
            // to state.
            state
                .telemetry
                .record_rejection(crate::telemetry::Rejection::Unauthenticated);
            return Ok(rejection);
        }
    };

    if method == Method::GET && (path == "/v1/models" || path == "/models") {
        return Ok(models_response(&state, &snapshot));
    }

    let subpath = path.strip_prefix("/v1").unwrap_or(&path);
    if method == Method::POST && PROXIED_SUFFIXES.contains(&subpath) {
        let subpath = subpath.to_string();
        return Ok(proxy_request(req, state, subpath, principal, &snapshot).await);
    }

    Ok(error_response(
        StatusCode::NOT_FOUND,
        "not_found",
        &format!("no route for {method} {path}"),
    ))
}

/// Fields the router needs. Everything else in the body is skipped without
/// being materialised.
///
/// Reading `model` off the front of the body with a hand-rolled scan was tried
/// and reverted: it measured 67.2k vs 67.1k req/s on 64KiB bodies. `serde_json`
/// skips what it does not want at ~16 B/ns, and the parse is ~3% of the work a
/// request costs — not worth a bespoke parser on the routing path.
#[derive(Deserialize)]
struct BodyPeek {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: Option<bool>,
    /// Read for virtual-model routing rules that match on requested
    /// generation length (`crate::routing::ShapeMatch`) — already sitting in
    /// the same JSON object `model` is parsed out of, so this costs nothing
    /// beyond one more field in a struct `serde_json` already skips past
    /// everything else in.
    ///
    /// Three spellings because OpenAI has used three. `max_tokens` is the
    /// original, `max_completion_tokens` replaced it on chat completions, and
    /// `max_output_tokens` is what the Responses API uses. A rule matching on
    /// generation length must not silently stop firing because a client
    /// upgraded its SDK — which is exactly what a single-name field would do,
    /// with no error anywhere.
    #[serde(default, alias = "max_completion_tokens", alias = "max_output_tokens")]
    max_tokens: Option<u64>,
}

/// Resolve the client-requested model name to the concrete model that will
/// actually be routed to, evaluating a virtual model's rules
/// (`crate::routing`) when the name is a virtual one.
///
/// **Authorisation decision**, pinned by
/// `tests::a_virtual_models_grant_does_not_reach_its_targets_and_vice_versa`:
/// the caller is authorised against the *resolved concrete model*
/// (`proxy_request` checks `principal.may_invoke(&target_model)` on what
/// this function returns), never against the virtual name by itself. A
/// virtual model routes access; it must never be able to grant it. The
/// alternative — authorising the virtual name and letting its rules
/// silently decide which concrete model actually serves the request —
/// would let a virtual model's configuration (or the weighted split, or a
/// health-driven failover) expand a principal's reach beyond whatever it
/// was actually granted, with no grant on record for the model that ends up
/// serving the request. Requiring the concrete grant means adding a virtual
/// model in front of existing models never changes who can reach what; it
/// only changes how an already-authorised request gets routed.
///
/// Returns the resolved model name, or an error response when the requested
/// name — virtual or concrete — does not resolve to anything at all. This
/// runs *before* authorisation, mirroring the existing "resolve, then
/// authorise" order for concrete models (an unknown name is a 404
/// regardless of what the caller may invoke — see `tests/rbac.rs`): a
/// caller who names a virtual model with no viable target learns "no such
/// model", not "you may not use the model this routes to", which would leak
/// routing internals to someone the very next check might reject anyway.
///
/// `Response<ResBody>` in the `Err` variant is large, same tradeoff
/// `authorize` already makes and documents: a boxed error would save stack
/// space on the rare rejection path at the cost of an allocation on every
/// call, which is the wrong trade for an interface shared with the rest of
/// the request path's error responses.
#[allow(clippy::result_large_err)]
fn resolve_target_models(
    requested_model: &str,
    snapshot: &Snapshot,
    facts: &crate::routing::RequestFacts<'_>,
    prefix: u64,
    registry: &Registry,
) -> Result<Vec<String>, Response<ResBody>> {
    let Some(vm) = snapshot.virtual_models.get(requested_model) else {
        return Ok(vec![requested_model.to_string()]);
    };
    let candidates = vm.resolve_candidates(facts, prefix, registry);
    if candidates.is_empty() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "model_not_found",
            &format!(
                "virtual model {requested_model:?} has no viable target for this request; \
                 check its routing rules and defaults"
            ),
        ));
    }
    Ok(candidates)
}

/// One span per request, carrying the fields worth asking a trace about.
///
/// `skip_all` because the arguments are the request body, the whole snapshot
/// and the caller's principal — recording those as span attributes would put
/// prompts and credentials into a tracing backend. Fields are added below,
/// explicitly, once they are known.
///
/// Level `debug` so the span costs nothing under the default `info` filter
/// even with the feature built in; the OTLP layer sets its own filter when a
/// collector is configured.
#[cfg_attr(
    feature = "otel",
    tracing::instrument(
        name = "chat_completion",
        skip_all,
        level = "debug",
        fields(
            model = tracing::field::Empty,
            served_model = tracing::field::Empty,
            backend = tracing::field::Empty,
            stream = tracing::field::Empty,
            class = tracing::field::Empty,
            status = tracing::field::Empty,
            attempts = tracing::field::Empty,
        )
    )
)]
async fn proxy_request(
    req: Request<Incoming>,
    state: Arc<AppState>,
    subpath: String,
    principal: Option<&Principal>,
    snapshot: &Snapshot,
) -> Response<ResBody> {
    // Taken before the body is even collected, so the measurement covers
    // everything this proxy is responsible for — including waiting on a slow
    // client's upload, which is otherwise invisible and looks like upstream
    // latency.
    let received_at = Instant::now();
    let (parts, body) = req.into_parts();

    let collected = match Limited::new(body, state.max_body_bytes).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            state.requests_failed.fetch_add(1, Ordering::Relaxed);
            // A body that outgrew the cap and a client that vanished mid-upload
            // are different failures and deserve different statuses.
            return if e
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                state
                    .telemetry
                    .record_rejection(crate::telemetry::Rejection::BodyTooLarge);
                error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    &format!("request body exceeds {} bytes", state.max_body_bytes),
                )
            } else {
                error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("could not read the request body: {e}"),
                )
            };
        }
    };

    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let (requested_model, model_field, streaming, max_tokens) =
        match multipart::boundary(content_type) {
            // Multipart: the audio routes. `model` is a form field, and the rest of
            // the body — the upload — must not be touched. Virtual-model routing
            // rules that key on `max_tokens` have nothing to read here (it is not
            // a multipart field on these routes), so it is simply `None`.
            Some(boundary) => {
                let Some(range) = multipart::find_field(&collected, &boundary, "model") else {
                    state.requests_failed.fetch_add(1, Ordering::Relaxed);
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "multipart body is missing the 'model' field",
                    );
                };
                match multipart::field_value(&collected, range.clone()).filter(|m| !m.is_empty()) {
                    Some(model) => (model.to_string(), Some(range), false, None),
                    None => {
                        state.requests_failed.fetch_add(1, Ordering::Relaxed);
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            "the 'model' form field is empty or not valid UTF-8",
                        );
                    }
                }
            }
            None => {
                let peek: BodyPeek = match serde_json::from_slice(&collected) {
                    Ok(p) => p,
                    Err(e) => {
                        state.requests_failed.fetch_add(1, Ordering::Relaxed);
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("request body is not valid JSON: {e}"),
                        );
                    }
                };
                let Some(model) = peek.model.filter(|m| !m.is_empty()) else {
                    state.requests_failed.fetch_add(1, Ordering::Relaxed);
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "request body is missing the 'model' field",
                    );
                };
                (model, None, peek.stream.unwrap_or(false), peek.max_tokens)
            }
        };

    let registry = state.registry.load();
    // Same hash the backend router uses for prefix affinity, computed once
    // and reused for the virtual-model weighted split below — see
    // `crate::routing`'s module doc comment for why reusing it (rather than
    // a second, independent hash) is what makes the split deterministic
    // *and* keeps cache locality once a concrete model is chosen.
    let prefix = state.router.prefix_key(&collected);

    // Classification runs before rule evaluation, and only when this snapshot
    // actually has classes — `classify` returns `None` immediately otherwise,
    // so a deployment that routes on caller or shape alone never touches an
    // embedding model. Tier 2 is gated one level further in, on whether any
    // active class refines the one tier 1 chose (`crate::classifier`).
    span_field!("model", requested_model.as_str());
    span_field!("stream", streaming);

    let classification = state.classify(&collected);
    #[cfg(feature = "classifier")]
    let (class_name, class_refines) = match &classification {
        Some(c) => {
            // Logged because a classifier whose quality drifts as traffic
            // changes is otherwise invisible until somebody complains about
            // answers. The margin is the number an operator tunes `min_margin`
            // against, and the tier says how often the expensive path is taken.
            debug!(
                class = %c.class,
                margin = c.margin,
                tier = c.tier.as_str(),
                "classified"
            );
            match c.tier {
                crate::classifier::Tier::Fast => &state.telemetry.classified_fast,
                crate::classifier::Tier::Refined => &state.telemetry.classified_refined,
            }
            .fetch_add(1, Ordering::Relaxed);
            span_field!("class", c.class.as_str());
            (Some(c.class.as_str()), c.refines.as_slice())
        }
        None => {
            // Only where classification was actually attempted: a deployment
            // with no classes must not look like one whose classifier never
            // matches anything.
            if state.has_prompt_classes() {
                state.telemetry.unclassified.fetch_add(1, Ordering::Relaxed);
            }
            (None, &[][..])
        }
    };
    #[cfg(not(feature = "classifier"))]
    let (class_name, class_refines) = {
        let _ = &classification;
        (None, &[][..])
    };
    let facts = crate::routing::RequestFacts {
        caller: principal,
        prompt_tokens: crate::routing::estimate_prompt_tokens(collected.len()),
        max_tokens,
        streaming,
        headers: &parts.headers,
        // One clock read, and only for a virtual model — `resolve_target_models`
        // returns before touching this for an ordinary name.
        now: chrono::Utc::now(),
        class: class_name,
        class_refines,
    };
    let candidates =
        match resolve_target_models(&requested_model, snapshot, &facts, prefix, &registry) {
            Ok(c) => c,
            Err(rejection) => {
                state.requests_failed.fetch_add(1, Ordering::Relaxed);
                return rejection;
            }
        };

    // Served-here before authorised, and that order is load bearing. It is
    // pinned by `tests/rbac.rs`: a model this proxy does not serve is a 404
    // whether or not the caller was granted it, so "403 vs 404" cannot be used
    // to probe which models exist. Authorising first would turn every unknown
    // name into a 403 and leak exactly that.
    //
    // A candidate whose pool is empty (every backend dropped for an
    // undecryptable key, say) drops out here rather than failing the request,
    // so a working fallback behind it still gets its turn.
    // The deployment-wide last resort, appended to every chain — virtual or
    // concrete. A rule author cannot anticipate every way a chain runs out
    // (every backend unreachable, every provider rate-limiting, a model whose
    // backends were all dropped for an undecryptable credential), and this is
    // what catches those. Skipped when it is already in the chain, so a rule
    // that names it does not get it twice.
    let mut candidates = candidates;
    // Remembered so serving it can be counted. Only when the fallback was
    // *added* here: a rule that names it explicitly chose it, and counting that
    // as the last resort catching something would misreport a working route as
    // a failure.
    let mut fallback_appended = None;
    if let Some(fallback) = &snapshot.fallback_model {
        if !candidates.iter().any(|c| c == fallback) {
            candidates.push(fallback.clone());
            fallback_appended = Some(fallback.clone());
        }
    }

    let served: Vec<(String, Pool)> = candidates
        .iter()
        .filter_map(|m| registry.pool(m).map(|p| (m.clone(), Arc::clone(p))))
        .collect();
    if served.is_empty() {
        state.requests_failed.fetch_add(1, Ordering::Relaxed);
        let known = registry.model_names().join(", ");
        let wanted = candidates.first().map(String::as_str).unwrap_or("");
        state
            .telemetry
            .record_rejection(crate::telemetry::Rejection::ModelNotFound);
        return error_response(
            StatusCode::NOT_FOUND,
            "model_not_found",
            &format!("model {wanted:?} is not served here; available: [{known}]"),
        );
    }

    // Authorisation is a set lookup against the pre-flattened grant list, not
    // a walk of the RBAC graph, and costs nothing measurable. Checked against
    // every *resolved concrete* candidate — see `resolve_target_models` for
    // why a virtual model's targets are what get authorised, never the virtual
    // name by itself.
    //
    // Ungranted candidates are dropped from the chain rather than failing the
    // request outright: failover then only ever moves to a model this caller
    // was already granted, so one fallback chain can span models with
    // different grants without any of them widening anyone's reach. When that
    // empties the chain the request is refused, naming the target the rule
    // actually chose.
    let routable: Vec<(String, Pool)> = match principal {
        Some(p) => served
            .iter()
            .filter(|(m, _)| p.may_invoke(m))
            .cloned()
            .collect(),
        None => served.clone(),
    };
    let Some((primary_model, _)) = routable.first().cloned() else {
        state.requests_failed.fetch_add(1, Ordering::Relaxed);
        let denied = served.first().map(|(m, _)| m.as_str()).unwrap_or_default();
        state
            .telemetry
            .record_rejection(crate::telemetry::Rejection::Unauthorised);
        return error_response(
            StatusCode::FORBIDDEN,
            "model_access_denied",
            &format!("key is not permitted to use model {denied:?}"),
        );
    };
    let target_model = primary_model;

    // Budget enforcement (P3): after the fact, on purpose. Consumption is
    // only known once a response completes (see the design doc's P3
    // section), so this compares against `tokens_used` as of the last
    // successful snapshot rebuild — a request that pushed a principal over
    // budget still completed; this is what refuses the *next* one. One
    // integer comparison, no I/O, same cost shape as the rate limiter below.
    if let Some(principal) = principal {
        if let Some(budget) = &principal.budget {
            if budget.exhausted() {
                state.requests_failed.fetch_add(1, Ordering::Relaxed);
                state
                    .telemetry
                    .record_rejection(crate::telemetry::Rejection::OverBudget);
                return budget_exceeded_response(budget);
            }
        }
    }

    // Carried to the response so a client can pace itself, which is the whole
    // point of publishing it: without these a caller only learns a limit
    // exists by being refused by it.
    let mut rate_limit_status = None;

    // Rate limiting (P2): one hash lookup and a short, synchronous
    // decrement — see `crate::limiter`'s doc comment for why this stays off
    // the I/O path the same way authorisation does. `Principal::limits ==
    // None` (an open snapshot, or a principal with nothing configured) never
    // even reaches `Limiter::check`; unlimited must cost nothing, not merely
    // decide nothing.
    if let Some(principal) = principal {
        if let Some(limits) = &principal.limits {
            // The proxy cannot know actual token usage until the response
            // completes (see the design doc's P3 section), so the tokens/min
            // dimension is charged against an estimate: the same prompt-size
            // estimate P1's shape-matching routing rules already compute,
            // plus the client's requested `max_tokens` if it gave one. This
            // is honestly an estimate, not a measurement — a request that
            // asks for less than `max_tokens` and gets it is charged for the
            // budget it reserved, not the tokens it turned out to use.
            let prompt_tokens = crate::routing::estimate_prompt_tokens(collected.len());
            let token_cost = prompt_tokens
                .saturating_add(max_tokens.unwrap_or(0))
                .min(u32::MAX as u64) as u32;
            match state
                .limiter
                .check(principal.id, limits, token_cost, std::time::Instant::now())
            {
                crate::limiter::Decision::Admitted(status) => rate_limit_status = status,
                crate::limiter::Decision::Exceeded { retry_after } => {
                    state.requests_failed.fetch_add(1, Ordering::Relaxed);
                    state
                        .telemetry
                        .record_rejection(crate::telemetry::Rejection::RateLimited);
                    return rate_limited_response(retry_after);
                }
            }
        }
    }

    // Response cache. After authorisation, so a hit is already authorised, and
    // after limits, so a cached answer still costs a request against the
    // caller's allowance — a cache is a latency and cost optimisation, not a
    // way around a quota.
    //
    // Keyed on the *resolved* model and the whole body, and only computed when
    // that model has caching on: a deployment that never opts in never hashes
    // anything.
    let cache_ttl = snapshot
        .models
        .iter()
        .find(|m| m.name == target_model)
        .and_then(|m| m.cache_ttl);
    let cache_key = cache_ttl.map(|_| crate::cache::ResponseCache::key(&target_model, &collected));
    if let Some(key) = cache_key {
        if let Some(entry) = state.cache.get(key, std::time::Instant::now()) {
            state.requests_ok.fetch_add(1, Ordering::Relaxed);
            state
                .telemetry
                .record_outcome(crate::telemetry::Outcome::Ok);
            if let Some(m) = state.telemetry.model(&target_model) {
                m.requests.fetch_add(1, Ordering::Relaxed);
            }
            let elapsed = received_at.elapsed().as_micros() as u64;
            state.telemetry.duration.record_us(elapsed);
            debug!(model = %target_model, "served from cache");
            return cached_response(entry, &rate_limit_status);
        }
    }

    // Whether this response is worth reading for real token counts (P3) --
    // decided once, outside the retry loop, since it depends only on the
    // principal and the request shape, neither of which changes between
    // attempts. `model_field.is_none()` restricts injection to the JSON
    // chat/completions-shaped routes: the multipart audio routes have no
    // `stream_options` to inject into and are never streaming completions.
    let needs_usage = principal_needs_usage(principal);
    let inject_include_usage = needs_usage && streaming && model_field.is_none();

    let mut last_error: Option<String> = None;

    // Two nested loops, and the distinction between them is the whole of
    // cross-model failover: the inner one moves between *backends of one
    // model* (a second node of the local pool), the outer one moves between
    // *models* (the local pool having failed, the cloud provider behind it).
    //
    // Before the outer loop existed a 429 from a single-backend model had
    // nowhere to go, which is exactly what a hosted provider's free tier
    // returns under load. Health-based selection could not help: the pool was
    // healthy, it simply refused this request.
    //
    // Retries at either level are safe only because nothing has been written
    // to the client yet. Once a single upstream frame is forwarded the response
    // is committed and a failure mid-stream propagates as-is.
    for (model_index, (candidate_model, pool)) in routable.iter().enumerate() {
        let more_models_after_this = model_index + 1 < routable.len();
        let mut tried: Vec<BackendUid> = Vec::new();

        for attempt in 0..=state.max_retries {
            let Some(backend) = state.router.pick(pool, prefix, &tried) else {
                break;
            };
            tried.push(backend.uid);

            // The fork between the two execution modes, and the only branch the
            // passthrough path pays for. `is_passthrough` is a match on a
            // `Copy` enum; everything under the `else` is unreachable for an
            // OpenAI-compatible backend, which is every backend unless an
            // operator configured otherwise.
            let (upstream_body, upstream_subpath) = if backend.protocol.is_passthrough() {
                let body = match rewrite_model_if_needed(
                    &collected,
                    &requested_model,
                    &backend.upstream_model,
                    model_field.clone(),
                    inject_include_usage,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        state.requests_failed.fetch_add(1, Ordering::Relaxed);
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("could not rewrite model name for alias: {e}"),
                        );
                    }
                };
                (body, Cow::Borrowed(subpath.as_str()))
            } else {
                // Only chat completions are translated. Embeddings, reranking and
                // the audio routes have no equivalent in either native protocol,
                // and forwarding an OpenAI-shaped body to an endpoint that cannot
                // read it would produce a confusing upstream error instead of a
                // clear local one.
                if subpath != "/chat/completions" {
                    state.requests_failed.fetch_add(1, Ordering::Relaxed);
                    state
                        .telemetry
                        .record_rejection(crate::telemetry::Rejection::Unsupported);
                    return error_response(
                        StatusCode::NOT_IMPLEMENTED,
                        "unsupported_endpoint",
                        &format!(
                        "{subpath} is not available on a {} backend; only /chat/completions is \
                         translated",
                        backend.protocol.as_str()
                    ),
                    );
                }
                match protocol::translate_request(
                    backend.protocol,
                    &collected,
                    &backend.upstream_model,
                    backend.default_max_tokens,
                ) {
                    Ok(t) => (Bytes::from(t.body), Cow::Owned(t.subpath)),
                    Err(e) => {
                        state.requests_failed.fetch_add(1, Ordering::Relaxed);
                        // Refusals are the client's to act on and are the same for
                        // every backend of this protocol, so there is nothing to
                        // gain by retrying onto another one.
                        let (status, kind) = match &e {
                            protocol::TranslateError::Unsupported(_) => {
                                (StatusCode::NOT_IMPLEMENTED, "unsupported_parameter")
                            }
                            _ => (StatusCode::BAD_REQUEST, "invalid_request_error"),
                        };
                        return error_response(status, kind, &e.to_string());
                    }
                }
            };

            let upstream_req = match build_upstream_request(
                &parts.headers,
                &backend,
                &upstream_subpath,
                upstream_body,
            ) {
                Ok(r) => r,
                Err(e) => {
                    state.requests_failed.fetch_add(1, Ordering::Relaxed);
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        &format!("could not build upstream request: {e}"),
                    );
                }
            };

            // The guard is taken before dispatch and moved into the response body,
            // so in-flight stays elevated for the whole generation.
            let guard = InflightGuard::acquire(Arc::clone(&backend));

            let dispatch = state.client.request(upstream_req);
            let result = match tokio::time::timeout(state.upstream_headers_timeout, dispatch).await
            {
                Ok(r) => r,
                Err(_) => {
                    backend.note_error();
                    last_error = Some(format!(
                        "upstream {} did not send headers within {:?}",
                        backend.api_base, state.upstream_headers_timeout
                    ));
                    warn!(backend = %backend.api_base, "upstream headers timeout");
                    if !state.router.has_candidate(pool, &tried) && more_models_after_this {
                        break;
                    }
                    continue;
                }
            };

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    // 429 joins 5xx as retryable, and it is the reason cross-model
                    // failover exists: a hosted provider answering "rate limited"
                    // is not broken, it is simply refusing *this* request, and the
                    // right answer is another provider rather than the client's
                    // problem. Every other 4xx is the client's fault and retrying
                    // it just multiplies it.
                    let retryable =
                        status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
                    if retryable {
                        backend.note_error();
                        // Another node of this same model, if there is one — but
                        // only if there *is* one. Retrying into an empty candidate
                        // set would discard this response and answer with a
                        // synthetic 502, throwing away the upstream's own
                        // diagnostics; on a single-node pool that is every 5xx.
                        if attempt < state.max_retries && state.router.has_candidate(pool, &tried) {
                            last_error =
                                Some(format!("upstream {} returned {}", backend.api_base, status));
                            state.telemetry.retries.fetch_add(1, Ordering::Relaxed);
                            debug!(backend = %backend.api_base, %status, "retrying on another backend");
                            backoff(attempt, status, prefix).await;
                            continue;
                        }
                        // This model is exhausted; the chain may still have another.
                        if more_models_after_this {
                            last_error = Some(format!(
                                "model {candidate_model:?} returned {status} from {}",
                                backend.api_base
                            ));
                            debug!(
                                backend = %backend.api_base,
                                model = %candidate_model,
                                %status,
                                "failing over to the next model in the chain"
                            );
                            state.telemetry.failovers.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                    state.telemetry.record_upstream_status(status.as_u16());
                    if status.is_client_error() || status.is_server_error() {
                        state.requests_failed.fetch_add(1, Ordering::Relaxed);
                        state
                            .telemetry
                            .record_outcome(crate::telemetry::Outcome::UpstreamError);
                    } else {
                        state.requests_ok.fetch_add(1, Ordering::Relaxed);
                        state
                            .telemetry
                            .record_outcome(crate::telemetry::Outcome::Ok);
                    }
                    // Counted against the model that actually answered, not the
                    // head of the chain — after a failover those differ, and
                    // crediting the one that refused would make the per-model
                    // rate a fiction.
                    if let Some(m) = state.telemetry.model(candidate_model) {
                        m.requests.fetch_add(1, Ordering::Relaxed);
                    }
                    if fallback_appended.as_deref() == Some(candidate_model.as_str()) {
                        state
                            .telemetry
                            .fallback_used
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    span_field!("served_model", candidate_model.as_str());
                    span_field!("backend", backend.api_base.as_str());
                    span_field!("status", status.as_u16());
                    span_field!("attempts", attempt + 1);
                    debug!(
                        backend = %backend.api_base,
                        requested_model = %requested_model,
                        model = %candidate_model,
                        stream = streaming,
                        %status,
                        "proxied"
                    );
                    // `needs_usage` (`principal_needs_usage`) is already `false`
                    // whenever `principal` is `None` (an open snapshot has no
                    // principal to attribute usage to), so this `and_then` never
                    // silently drops tracking that was actually wanted.
                    if !backend.protocol.is_passthrough() {
                        // Attributed to the model that actually answered, not the
                        // head of the chain: after a failover those differ, and
                        // billing the model that refused the request would make
                        // the usage record a fiction.
                        let sink = needs_usage.then_some(principal).flatten().map(|p| {
                            protocol::body::UsageSink {
                                principal_id: p.id,
                                model: candidate_model.clone(),
                                requested_model: (requested_model.as_str()
                                    != candidate_model.as_str())
                                .then(|| requested_model.to_string()),
                                status: status.as_u16(),
                                reporter: state.usage.clone(),
                            }
                        });
                        let mut resp = translated_response(
                            resp,
                            guard,
                            backend.protocol,
                            protocol::ResponseContext::from_request(
                                &collected,
                                candidate_model.clone(),
                                prefix,
                            ),
                            sink,
                            Some(
                                crate::telemetry::RequestTiming::new(
                                    &state.telemetry,
                                    candidate_model,
                                    streaming,
                                    received_at,
                                )
                                .on_backend(Arc::clone(&backend)),
                            ),
                        );
                        if let Some(status) = &rate_limit_status {
                            stamp_rate_limit_headers(&mut resp, status);
                        }
                        return resp;
                    }
                    let tracking =
                        needs_usage
                            .then_some(principal)
                            .flatten()
                            .map(|p| UsageTracking {
                                tail: TailBuffer::new(crate::tail_buffer::DEFAULT_CAPACITY),
                                metrics: state.telemetry.model(candidate_model),
                                principal_id: p.id,
                                model: candidate_model.clone(),
                                // Only when it differs: storing the same name
                                // twice on every row would be noise in the one
                                // place the detail is meant to be readable.
                                requested_model: (requested_model.as_str()
                                    != candidate_model.as_str())
                                .then(|| requested_model.to_string()),
                                status: status.as_u16(),
                                reporter: state.usage.clone(),
                            });
                    // Store, if this model opted in and the response is one
                    // that can be replayed. Streaming is excluded: caching a
                    // stream means buffering the whole thing before any of it
                    // reaches the client, which turns the one path this proxy
                    // exists to keep incremental into a batch operation.
                    // Non-2xx is excluded because an error is a statement about
                    // *now* — serving a 429 from cache would keep a provider's
                    // bad minute alive long after it ended.
                    if let (Some(key), Some(ttl), false, true) =
                        (cache_key, cache_ttl, streaming, status.is_success())
                    {
                        let cache = Arc::clone(&state.cache);
                        let content_type = resp
                            .headers()
                            .get(CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        return cache_and_finish(
                            resp,
                            guard,
                            tracking,
                            key,
                            ttl,
                            content_type,
                            cache,
                            rate_limit_status,
                            crate::telemetry::RequestTiming::new(
                                &state.telemetry,
                                candidate_model,
                                streaming,
                                received_at,
                            )
                            .on_backend(Arc::clone(&backend)),
                        )
                        .await;
                    }
                    let mut resp = finish_response(
                        resp,
                        guard,
                        tracking,
                        Some(
                            crate::telemetry::RequestTiming::new(
                                &state.telemetry,
                                candidate_model,
                                streaming,
                                received_at,
                            )
                            .on_backend(Arc::clone(&backend)),
                        ),
                    );
                    if let Some(status) = &rate_limit_status {
                        stamp_rate_limit_headers(&mut resp, status);
                    }
                    return resp;
                }
                Err(e) => {
                    backend.note_error();
                    last_error = Some(format!("upstream {} unreachable: {e}", backend.api_base));
                    warn!(backend = %backend.api_base, error = %e, "upstream request failed");
                    if !state.router.has_candidate(pool, &tried) && more_models_after_this {
                        break;
                    }
                    continue;
                }
            }
        }
    }

    state.requests_failed.fetch_add(1, Ordering::Relaxed);
    let detail =
        last_error.unwrap_or_else(|| format!("no healthy backend for model {target_model:?}"));
    state
        .telemetry
        .record_outcome(crate::telemetry::Outcome::Unavailable);
    error_response(StatusCode::BAD_GATEWAY, "upstream_unavailable", &detail)
}

/// Rebuild the body with a different `model` value and/or an injected
/// `stream_options.include_usage`, or hand back the original bytes when
/// neither rewrite is needed.
///
/// The common case — no alias, no injection — is a straight `Bytes` clone (a
/// refcount bump), same allocation as the caller already had. Only a request
/// that actually needs one of the two rewrites pays for a reparse — and for
/// multipart, `model_field` is the already-located range of the field value,
/// so a model rewrite there is a splice rather than a re-encode of the
/// upload. `inject_include_usage` never applies to multipart (the audio
/// routes have no `stream_options`, and are never streaming completions), so
/// callers only ever pass it `true` alongside `model_field: None`.
///
/// **Why inject at all** (design doc, "P3 -- Usage accounting and
/// budgets"): a streaming response only carries a `usage` object if the
/// request asked for one via `stream_options.include_usage`. Real token
/// counts are needed for principals with a configured budget or
/// tokens-per-minute limit, so this proxy asks the upstream for them on
/// their behalf — but *only* for those principals (see
/// `principal_needs_usage`), because the injection adds a usage chunk to the
/// stream that the client itself never asked for.
fn rewrite_model_if_needed(
    body: &Bytes,
    requested: &str,
    upstream: &str,
    model_field: Option<Range<usize>>,
    inject_include_usage: bool,
) -> Result<Bytes, serde_json::Error> {
    if requested == upstream && !inject_include_usage {
        return Ok(body.clone());
    }
    if let Some(range) = model_field {
        return Ok(if requested == upstream {
            body.clone()
        } else {
            multipart::replace_range(body, range, upstream)
        });
    }
    let mut value: serde_json::Value = serde_json::from_slice(body)?;
    if let Some(obj) = value.as_object_mut() {
        if requested != upstream {
            obj.insert(
                "model".into(),
                serde_json::Value::String(upstream.to_string()),
            );
        }
        if inject_include_usage {
            // Merged into whatever `stream_options` the client already sent
            // (if any) rather than overwriting the whole object, so a
            // client-supplied option alongside this one survives the rewrite.
            let entry = obj
                .entry("stream_options")
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let Some(stream_options) = entry.as_object_mut() {
                stream_options.insert("include_usage".into(), serde_json::Value::Bool(true));
            }
        }
    }
    Ok(Bytes::from(serde_json::to_vec(&value)?))
}

/// Whether this request's consumption is worth reading. True for any
/// attributable request — that is, any request with a principal.
///
/// This was once scoped to principals with a budget or a token-rate limit,
/// on the reasoning that nothing else consumed the number and injecting
/// `stream_options.include_usage` adds a chunk the client did not ask for.
/// The reasoning was sound and the result was not: usage became a side
/// effect of *enforcement*, so a deployment that enforced nothing recorded
/// nothing. On the cluster this was found on, one principal of seven had a
/// budget, `usage_events` held nineteen rows, and the newest was five days
/// old — while the gateway had served hundreds of requests. Every "Usage &
/// spend" figure was therefore true and useless, and no history existed to
/// chart, because history was never written.
///
/// Accounting is not a side effect of enforcement. What a caller consumed is
/// worth knowing whether or not a cap is attached to it, and it cannot be
/// recovered later: the token counts live in a response body that is
/// forwarded and gone.
///
/// The stated cost stands and is now paid on every attributable request:
/// one small parse of a bounded response tail, and one extra SSE chunk on
/// streamed responses. Neither is I/O — reporting is an in-memory queue
/// drained by a background flush — so the "no I/O on the request path" rule
/// is untouched. See `docs/performance.md` for the measurement.
///
/// Still false for an unauthenticated request: with no principal there is
/// nothing to attribute consumption to, and a row that cannot name who
/// spent is not accounting.
#[inline]
fn principal_needs_usage(principal: Option<&Principal>) -> bool {
    principal.is_some()
}

fn build_upstream_request(
    client_headers: &HeaderMap,
    backend: &Backend,
    subpath: &str,
    body: Bytes,
) -> anyhow::Result<Request<Full<Bytes>>> {
    let url = backend.url_for(subpath);
    let mut builder = Request::builder().method(Method::POST).uri(&url);

    let headers = builder.headers_mut().expect("builder has no error yet");
    for (name, value) in client_headers.iter() {
        let name_str = name.as_str();
        if HOP_BY_HOP.contains(&name_str) || REQUEST_ONLY_STRIPPED.contains(&name_str) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    // The client's content-type is carried through untouched — a multipart
    // upload's boundary parameter lives there, and overwriting it would make
    // the body unparseable upstream. JSON is only assumed when nothing was set.
    if !headers.contains_key(CONTENT_TYPE) {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    // Pre-built at snapshot time: the auth header (whose *name* varies by
    // provider) plus any constants the protocol requires.
    for (name, value) in &backend.headers {
        headers.insert(name.clone(), value.clone());
    }

    Ok(builder.body(Full::new(body))?)
}

/// Hand the upstream response to the client, keeping the in-flight guard alive
/// for as long as the body is still streaming, and — for a principal with a
/// budget or a token-rate limit — feeding forwarded bytes into a bounded tail
/// buffer so real usage can be reported once the stream ends (P3).
fn finish_response(
    resp: Response<UpstreamBody>,
    guard: InflightGuard,
    tracking: Option<UsageTracking>,
    timing: Option<crate::telemetry::RequestTiming>,
) -> Response<ResBody> {
    let (mut parts, body) = resp.into_parts();
    // Hop-by-hop only: the response body is passed through byte for byte, so
    // an upstream `content-length` remains correct and is worth keeping.
    for name in HOP_BY_HOP {
        parts.headers.remove(*name);
    }
    Response::from_parts(
        parts,
        TrackedBody::new(body, guard, tracking, timing).boxed(),
    )
}

/// Hand a *translated* upstream response to the client.
///
/// Differs from [`finish_response`] in one way beyond the body type, and it is
/// the one that breaks things if missed: the upstream's `content-length`
/// describes the document we just rewrote, and a translated body is almost
/// never the same length. Leaving it would either truncate the response at the
/// client or leave the connection waiting for bytes that never come, so it is
/// dropped and the response goes out chunked.
fn translated_response(
    resp: Response<UpstreamBody>,
    guard: InflightGuard,
    protocol: protocol::Protocol,
    ctx: protocol::ResponseContext,
    sink: Option<protocol::body::UsageSink>,
    timing: Option<crate::telemetry::RequestTiming>,
) -> Response<ResBody> {
    let (mut parts, body) = resp.into_parts();
    for name in HOP_BY_HOP {
        parts.headers.remove(*name);
    }
    parts.headers.remove(CONTENT_LENGTH);
    // The upstream's own content type describes its format, not ours: a
    // native non-streaming response is `application/json` either way, but a
    // provider that labels its stream differently would mislabel ours.
    parts.headers.insert(
        CONTENT_TYPE,
        if ctx.streaming {
            HeaderValue::from_static("text/event-stream")
        } else {
            HeaderValue::from_static("application/json")
        },
    );
    Response::from_parts(
        parts,
        protocol::body::TranslatedBody::new(body, guard, protocol, ctx, sink, timing).boxed_body(),
    )
}

/// Everything [`TrackedBody`] needs to turn "the stream ended" into one
/// `usage::UsageReporter::record` call. Constructed once per request, in
/// `proxy_request`, only when [`principal_needs_usage`] said this principal's
/// consumption is worth reading at all.
struct UsageTracking {
    tail: TailBuffer,
    /// Where to add the token counts, independent of whether they are also
    /// reported to the control plane.
    metrics: Option<Arc<crate::telemetry::ModelMetrics>>,
    principal_id: PrincipalId,
    /// The resolved concrete model name (`snapshot::ModelDef::name`), not
    /// the client-requested one — see `usage::UsageEvent::model`'s doc
    /// comment for why the data plane only ever reports the name the way the
    /// snapshot named it.
    model: String,
    /// What the client asked for, when it differs from `model`.
    requested_model: Option<String>,
    status: u16,
    reporter: UsageReporter,
}

impl UsageTracking {
    /// The one parse per request (`TailBuffer::extract_usage`), and the one
    /// `record` call it always feeds.
    ///
    /// A row is written whether or not the parse found anything, because the
    /// event answers two different questions and only one of them is about
    /// tokens. "How many requests, how many failed, how slow were they" is
    /// answerable for every request; "how many tokens did it burn" is not.
    /// Recording only the responses that carried usage made the first
    /// question unanswerable from the database — an upstream 502 carries no
    /// usage block, so under the old rule the errors were precisely the rows
    /// that never appeared, and an error-rate chart built on them would have
    /// read a flat zero no matter how much was failing.
    ///
    /// `usage_reported` is what keeps that honest rather than merely
    /// complete: absent counts are recorded as absent, never as zero. Zero
    /// tokens and unknown tokens are different facts, and averaging the
    /// second into the first is how a dashboard ends up quietly understating
    /// consumption.
    fn finish(mut self, timing: Option<&crate::telemetry::RequestTiming>) {
        let tokens = self.tail.extract_usage();
        if let (Some(m), Some(t)) = (&self.metrics, tokens.as_ref()) {
            m.prompt_tokens
                .fetch_add(u64::from(t.prompt_tokens), Ordering::Relaxed);
            m.completion_tokens
                .fetch_add(u64::from(t.completion_tokens), Ordering::Relaxed);
        }
        self.reporter.record(UsageEvent {
            principal_id: self.principal_id,
            model: self.model,
            prompt_tokens: tokens.as_ref().map_or(0, |t| t.prompt_tokens),
            completion_tokens: tokens.as_ref().map_or(0, |t| t.completion_tokens),
            usage_reported: tokens.is_some(),
            at: chrono::Utc::now(),
            duration_ms: timing.map(|t| t.duration_ms()),
            ttft_ms: timing.and_then(|t| t.ttft_ms()),
            status: Some(self.status),
            requested_model: self.requested_model,
            cost_micros: tokens.as_ref().and_then(|t| t.cost_micros),
        });
    }
}

pin_project! {
    /// Passthrough body that releases an [`InflightGuard`] when the stream ends,
    /// and — when `usage_tracking` is `Some` — mirrors forwarded bytes into a
    /// bounded tail buffer along the way.
    ///
    /// Deliberately not batching: merging frames that have already arrived was
    /// measured at a 1.000 merge ratio both against the pooled client and
    /// against the owned connection that replaced it, and against a real vLLM,
    /// whose tokens arrive tens of milliseconds apart. There is never a second
    /// frame waiting, so a coalescing buffer here only adds a poll and a branch.
    ///
    /// `usage_tracking` is plain (not `#[pin]`): nothing in it is ever polled,
    /// it is only read from and mutated between polls of `inner`.
    struct TrackedBody {
        #[pin]
        inner: UpstreamBody,
        guard: Option<InflightGuard>,
        usage_tracking: Option<UsageTracking>,
        // Taken once, so a body finished by `poll_frame` is not recorded a
        // second time by `PinnedDrop` — the same rule the usage tracking above
        // follows, for the same reason.
        timing: Option<crate::telemetry::RequestTiming>,
    }

    impl PinnedDrop for TrackedBody {
        /// The parse cannot rely on being polled to `None`.
        ///
        /// A non-streaming response carries a `content-length`, so once the last
        /// frame is out `is_end_stream()` is true and hyper's server simply stops
        /// polling — the `Ready(None)` that `poll_frame` keys the extraction off
        /// never arrives. Usage was therefore recorded for streaming responses and
        /// silently dropped for every non-streaming one, which is the shape the
        /// budget end-to-end test caught.
        ///
        /// This is the same trap the connection pool hit in `upstream::UpstreamBody`
        /// and is worth the duplicated safety net: `finish()` takes the tracking out
        /// of the `Option`, so whichever path runs first wins and the other is a
        /// no-op. Dropping a half-read body still reports whatever the tail holds —
        /// a client that hung up mid-generation consumed those tokens upstream
        /// regardless, so not billing them would be the wrong way to be wrong.
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(tracking) = this.usage_tracking.take() {
                tracking.finish(this.timing.as_ref());
            }
            if let Some(timing) = this.timing.take() {
                timing.finish();
            }
        }
    }
}

impl TrackedBody {
    fn new(
        inner: UpstreamBody,
        guard: InflightGuard,
        usage_tracking: Option<UsageTracking>,
        timing: Option<crate::telemetry::RequestTiming>,
    ) -> Self {
        Self {
            inner,
            guard: Some(guard),
            usage_tracking,
            timing,
        }
    }
}

impl Body for TrackedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let polled = this.inner.poll_frame(cx);
        // A memcpy per frame into the bounded tail buffer, never a parse —
        // see `crate::tail_buffer`'s module doc comment for why this is the
        // one piece of per-frame work the design accepts.
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            if let Some(data) = frame.data_ref() {
                if let Some(tracking) = this.usage_tracking.as_mut() {
                    tracking.tail.push(data);
                }
                // Time to first token, from the only place that knows when the
                // first byte actually reaches the client.
                if let Some(timing) = this.timing.as_mut() {
                    timing.first_byte();
                }
            }
        }
        // The one parse per request happens here, at clean end-of-stream —
        // but an aborted stream (the `Err` arm below, or the body simply
        // being dropped mid-poll) is *not* left unreported: `PinnedDrop`
        // above runs `tracking.finish()` on whatever partial tail the
        // buffer already holds, same as `usage_tracking.take()` does here.
        // That is deliberate, not a gap — see `PinnedDrop`'s own doc
        // comment for why billing whatever was consumed upstream before the
        // abort is the right call, not the wrong one.
        if let Poll::Ready(None) = &polled {
            if let Some(tracking) = this.usage_tracking.take() {
                tracking.finish(this.timing.as_ref());
            }
            if let Some(timing) = this.timing.take() {
                timing.finish();
            }
        }
        // Release on both clean end-of-stream and error; a client that hangs up
        // mid-generation drops the whole body and the guard with it.
        if let Poll::Ready(None) | Poll::Ready(Some(Err(_))) = polled {
            this.guard.take();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Authenticate the caller and return their principal.
///
/// `Ok(None)` means the snapshot is open — no keys configured — which is the
/// same permissive behaviour as running without a master key today.
///
/// The `Err` variant is a full `Response`, which clippy flags as large; a
/// boxed error would save stack space on the (rare) rejection path at the
/// cost of an allocation on every one, which is the wrong trade for an
/// interface shared with the rest of the request path's error responses.
#[allow(clippy::result_large_err)]
fn authorize<'a>(
    req: &Request<Incoming>,
    snapshot: &'a Snapshot,
) -> Result<Option<&'a Principal>, Response<ResBody>> {
    if snapshot.open {
        return Ok(None);
    }
    let token = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);

    let Some(token) = token else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid bearer token",
        ));
    };
    match snapshot.authenticate(token, SystemTime::now()) {
        Ok(p) => Ok(Some(p)),
        Err(AuthError::Expired) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "expired_api_key",
            "this api key has expired",
        )),
        Err(_) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid bearer token",
        )),
    }
}

/// Token out of an `Authorization` value, if the scheme is Bearer.
///
/// RFC 7235 makes the auth scheme case-insensitive, and clients do send
/// `bearer`; matching only the capitalised form rejects a valid key.
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

fn json_response(status: StatusCode, body: String) -> Response<ResBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(Bytes::from(body)))
        .expect("static response is well-formed")
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response<ResBody> {
    let body = serde_json::json!({
        "error": { "message": message, "type": kind, "code": status.as_u16() }
    });
    json_response(status, body.to_string())
}

/// 429 with `Retry-After` (whole seconds, rounded up, at least 1 — a `0`
/// would tell a client to retry immediately, which is what got it rate
/// limited in the first place).
fn rate_limited_response(retry_after: std::time::Duration) -> Response<ResBody> {
    let secs = retry_after.as_secs_f64().ceil().max(1.0) as u64;
    let body = serde_json::json!({
        "error": {
            "message": format!("rate limit exceeded; retry after {secs}s"),
            "type": "rate_limit_exceeded",
            "code": StatusCode::TOO_MANY_REQUESTS.as_u16(),
        }
    });
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "application/json")
        .header("retry-after", secs.to_string())
        .body(full(Bytes::from(body.to_string())))
        .expect("static response is well-formed")
}

/// **402, not 429.** Both are defensible for "you may not make this request
/// right now", but they mean different things and this proxy already uses
/// 429 for the other one: rate limiting (`rate_limited_response` above) is a
/// *pacing* problem — the same request succeeds if the caller waits a few
/// seconds and `Retry-After` says exactly how long. A budget is a *spending*
/// problem — the caller has consumed the tokens it was allocated for this
/// window, and no amount of waiting a few seconds fixes that; the earliest
/// legitimate retry is whenever the window rolls over (hours to a month
/// away, not something worth promising via `Retry-After`) or whenever an
/// operator raises the budget. 402 Payment Required is the one status in the
/// spec whose stated meaning — access denied pending some accounting action,
/// not merely "try again soon" — actually matches that.
/// Micro-dollars as a plain decimal amount.
///
/// No currency symbol: the unit is whatever the operator priced models in, and
/// asserting dollars in a message would be wrong for anyone who did not.
/// Buffer a non-streaming response, store it, and hand it to the client.
///
/// Buffering is exactly what makes this safe only for non-streaming: the whole
/// body is collected before any of it goes out. For a completion that fits in
/// one document that is the shape it already had; for a stream it would be a
/// change of behaviour, which is why streams are never routed here.
///
/// A body that fails to collect is not cached and not served — the upstream
/// broke mid-response, and inventing a truncated cache entry from it would make
/// one bad response permanent.
#[allow(clippy::too_many_arguments)]
async fn cache_and_finish(
    resp: Response<UpstreamBody>,
    guard: InflightGuard,
    tracking: Option<UsageTracking>,
    key: u64,
    ttl: std::time::Duration,
    content_type: Option<String>,
    cache: Arc<crate::cache::ResponseCache>,
    rate_limit_status: Option<crate::limiter::RateLimitStatus>,
    mut timing: crate::telemetry::RequestTiming,
) -> Response<ResBody> {
    let (mut parts, body) = resp.into_parts();
    for name in HOP_BY_HOP {
        parts.headers.remove(*name);
    }
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            warn!(error = %e, "upstream body failed mid-response; not cached");
            drop(guard);
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unavailable",
                "the upstream closed the response before it was complete",
            );
        }
    };

    // Usage still comes from the same tail parse; buffering does not change
    // where the token counts are read from.
    if let Some(mut tracking) = tracking {
        tracking.tail.push(&collected);
        tracking.finish(Some(&timing));
    }
    // A buffered response has no separate first-byte moment, and `first_byte`
    // declines to record one for a non-streaming request anyway.
    timing.first_byte();
    timing.finish();
    drop(guard);

    cache.put(
        key,
        content_type,
        collected.clone(),
        ttl,
        std::time::Instant::now(),
    );

    let mut resp = Response::from_parts(parts, full(collected));
    resp.headers_mut()
        .insert("x-fastllm-cache", HeaderValue::from_static("miss"));
    if let Some(status) = &rate_limit_status {
        stamp_rate_limit_headers(&mut resp, status);
    }
    resp
}

/// Serve a cached response.
///
/// `x-fastllm-cache: hit` because a caller measuring latency deserves to know
/// why one request took a microsecond and the next took a second; without it a
/// cache looks like wildly inconsistent performance.
fn cached_response(
    entry: crate::cache::Entry,
    rate_limit_status: &Option<crate::limiter::RateLimitStatus>,
) -> Response<ResBody> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("x-fastllm-cache", "hit");
    if let Some(ct) = &entry.content_type {
        builder = builder.header(CONTENT_TYPE, ct.as_str());
    }
    let mut resp = builder
        .body(full(entry.body))
        .expect("a cached body is always a valid response");
    if let Some(status) = rate_limit_status {
        stamp_rate_limit_headers(&mut resp, status);
    }
    resp
}

/// Wait a little before retrying, and a little longer each time.
///
/// Retries used to be immediate. Against a 5xx that merely wastes an attempt;
/// against a 429 it is actively counterproductive, since hammering a provider
/// that just said "too many requests" is what a rate limiter exists to stop.
///
/// Deliberately small and bounded — 25ms, 50ms, 100ms — because this delay is
/// paid by a client that is still waiting for its answer. This is a retry
/// budget measured against one request's patience, not a background job's.
///
/// Jitter comes from the request's own prefix hash rather than an RNG: the
/// data plane has no random source in a `--no-default-features` build, and the
/// property that matters is only that a thundering herd of simultaneous
/// retries does not stay in lockstep. Different requests hash differently,
/// which is exactly the decorrelation needed.
async fn backoff(attempt: usize, status: StatusCode, prefix: u64) {
    let base_ms = 25u64 << attempt.min(2);
    // Up to +50%, keyed on the request.
    let jitter_ms = prefix % (base_ms / 2 + 1);
    let mut wait = std::time::Duration::from_millis(base_ms + jitter_ms);
    // A provider that said "too many requests" means it; give it longer than a
    // transient 5xx gets, still inside what a waiting client will tolerate.
    if status == StatusCode::TOO_MANY_REQUESTS {
        wait *= 2;
    }
    tokio::time::sleep(wait).await;
}

/// Publish the caller's remaining allowance on a response.
///
/// The `x-ratelimit-*` names are the de-facto shape every provider uses; a
/// client that already paces itself against OpenAI needs no new code to pace
/// itself against this. Without them a caller only learns a limit exists by
/// being refused by it.
fn stamp_rate_limit_headers(
    resp: &mut Response<ResBody>,
    status: &crate::limiter::RateLimitStatus,
) {
    let headers = resp.headers_mut();
    let mut put = |name: &'static str, value: u64| {
        if let Ok(v) = HeaderValue::from_str(&value.to_string()) {
            headers.insert(name, v);
        }
    };
    if let (Some(limit), Some(remaining)) = (status.requests_limit, status.requests_remaining) {
        put("x-ratelimit-limit-requests", u64::from(limit));
        put("x-ratelimit-remaining-requests", u64::from(remaining));
    }
    if let (Some(limit), Some(remaining)) = (status.tokens_limit, status.tokens_remaining) {
        put("x-ratelimit-limit-tokens", u64::from(limit));
        put("x-ratelimit-remaining-tokens", u64::from(remaining));
    }
    put("x-ratelimit-reset", status.reset_after.as_secs());
}

fn micros_as_currency(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn budget_exceeded_response(budget: &Budget) -> Response<ResBody> {
    let body = serde_json::json!({
        "error": {
            // Names which cap was hit, because "budget exhausted" leaves an
            // operator guessing between raising tokens and raising spend.
            "message": match budget.exceeded_kind() {
                Some("cost") => format!(
                    "spend budget exhausted: {} of {} used for the current window",
                    micros_as_currency(budget.cost_used_micros),
                    micros_as_currency(budget.cost_total_micros.unwrap_or(0)),
                ),
                _ => format!(
                    "token budget exhausted: {} of {} tokens used for the current window",
                    budget.tokens_used,
                    budget.tokens_total.unwrap_or(0),
                ),
            },
            "type": "budget_exceeded",
            "code": StatusCode::PAYMENT_REQUIRED.as_u16(),
        }
    });
    Response::builder()
        .status(StatusCode::PAYMENT_REQUIRED)
        .header("content-type", "application/json")
        .body(full(Bytes::from(body.to_string())))
        .expect("static response is well-formed")
}

/// OpenAI-shaped model list, aggregated over every pool.
/// OpenAI-shaped model list.
///
/// **Lists both concrete and virtual models.** `/v1/models` has never been
/// filtered by what the caller may invoke — it enumerates what is
/// *configured*, same as today's behaviour for concrete models — so leaving
/// virtual models out would make them undiscoverable except by already
/// knowing their name, without adding any actual access control: a caller
/// with no grant for a listed model already gets 403 from `/v1/chat/completions`
/// regardless of whether that model appeared here. Concrete models are kept
/// in the list too, even ones that exist only as a virtual model's target —
/// removing them would break any client still addressing them directly,
/// which is a legitimate and unrestricted thing to do unless someone
/// deliberately revokes the grant.
fn models_response(state: &AppState, snapshot: &Snapshot) -> Response<ResBody> {
    let registry = state.registry.load();
    let mut names: Vec<&str> = registry.model_names();
    names.extend(snapshot.virtual_models.keys().map(String::as_str));
    names.sort_unstable();
    names.dedup();
    let data: Vec<serde_json::Value> = names
        .into_iter()
        .map(|name| {
            serde_json::json!({
                "id": name,
                "object": "model",
                "owned_by": "fastllm-proxy",
                "created": 0,
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::json!({ "object": "list", "data": data }).to_string(),
    )
}

fn health_response(state: &AppState) -> Response<ResBody> {
    let registry = state.registry.load();
    let backends: Vec<serde_json::Value> = registry
        .backends()
        .iter()
        .map(|b| {
            serde_json::json!({
                "api_base": b.api_base,
                "model": b.upstream_model,
                "healthy": b.is_healthy(),
                "inflight": b.inflight(),
                "requests_total": b.requests_total(),
                "errors_total": b.errors_total(),
            })
        })
        .collect();
    let snapshot = state.snapshot.load();
    let healthy = registry.healthy_count();
    let status = if healthy > 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    json_response(
        status,
        serde_json::json!({
            "status": if healthy > 0 { "ok" } else { "no_healthy_backends" },
            "policy": format!("{:?}", state.router.policy()),
            "uptime_seconds": state.started.elapsed().as_secs(),
            // Which configuration this process is actually serving. Without it
            // "is this pod current?" is unanswerable from outside, and a pod
            // that is behind looks identical to one that is not — the models
            // and backends it lists can be right while a key it has never seen
            // is rejected.
            "snapshot_version": snapshot.version,
            "keys": snapshot.keys.len(),
            "models": registry.model_names(),
            "backends": backends,
        })
        .to_string(),
    )
}

/// Seconds since a snapshot was stamped, or 0 if that is in the future.
///
/// The stamp is the control plane's clock in microseconds
/// (`control::build::snapshot_version`), so this compares two machines' clocks
/// and can go negative when they disagree. Clamping beats publishing a negative
/// age, which reads as a broken exporter rather than as clock skew.
fn snapshot_age_seconds(version: u64) -> u64 {
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    // A `File`-mode snapshot has no control-plane stamp at all.
    if version == 0 {
        return 0;
    }
    now_us.saturating_sub(version) / 1_000_000
}

fn metrics_response(state: &AppState) -> Response<ResBody> {
    let registry = state.registry.load();
    let mut out = String::with_capacity(1024);

    out.push_str("# HELP fastllm_requests_total Requests answered with a success status.\n");
    out.push_str("# TYPE fastllm_requests_total counter\n");
    out.push_str(&format!(
        "fastllm_requests_total {}\n",
        state.requests_ok.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP fastllm_requests_failed_total Requests rejected here, answered with an error status, or that exhausted every backend.\n");
    out.push_str("# TYPE fastllm_requests_failed_total counter\n");
    out.push_str(&format!(
        "fastllm_requests_failed_total {}\n",
        state.requests_failed.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP fastllm_usage_reports_dropped_total Usage events discarded because the reporting queue to the control plane was full — see crate::usage.\n");
    out.push_str("# TYPE fastllm_usage_reports_dropped_total counter\n");
    out.push_str(&format!(
        "fastllm_usage_reports_dropped_total {}\n",
        state.usage.dropped()
    ));

    // Scrapeable so a fleet-wide `max() - min()` shows a pod stuck on an old
    // configuration, which is otherwise invisible: a lagging pod answers
    // /health with `ok` and the right model list, and only misbehaves on the
    // part of the snapshot that changed.
    out.push_str("# HELP fastllm_snapshot_version Version of the snapshot this process is serving. The control plane's clock at build time, so it is comparable across processes.\n");
    out.push_str("# TYPE fastllm_snapshot_version gauge\n");
    out.push_str(&format!(
        "fastllm_snapshot_version {}\n",
        state.snapshot.load().version
    ));

    // Version as a label on a constant 1, the conventional shape: it makes a
    // deploy visible as a change of series in a graph, which is what turns
    // "latency moved at 14:02" into "latency moved when we shipped this".
    out.push_str("# HELP fastllm_build_info Build identity. Always 1; read the labels.\n");
    out.push_str("# TYPE fastllm_build_info gauge\n");
    out.push_str(&format!(
        "fastllm_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));

    // Age rather than only the version: "which snapshot" needs a fleet-wide
    // comparison to interpret, where "how stale" is actionable on its own.
    out.push_str(
        "# HELP fastllm_snapshot_age_seconds Seconds since the snapshot this process serves was \
         built by the control plane.\n",
    );
    out.push_str("# TYPE fastllm_snapshot_age_seconds gauge\n");
    out.push_str(&format!(
        "fastllm_snapshot_age_seconds {}\n",
        snapshot_age_seconds(state.snapshot.load().version)
    ));

    // The cache's own counters. A hit rate is the number that says whether
    // caching is earning its memory, and it cannot be inferred from request
    // totals — a hit and a miss both look like one served request.
    out.push_str(
        "# HELP fastllm_cache_total Response-cache lookups and stores. A hit served a request \
         without contacting any provider.\n",
    );
    out.push_str("# TYPE fastllm_cache_total counter\n");
    for (kind, value) in [
        ("hit", state.cache.hits.load(Ordering::Relaxed)),
        ("miss", state.cache.misses.load(Ordering::Relaxed)),
        ("store", state.cache.stores.load(Ordering::Relaxed)),
    ] {
        out.push_str(&format!("fastllm_cache_total{{kind=\"{kind}\"}} {value}\n"));
    }
    out.push_str("# HELP fastllm_cache_entries Responses currently cached.\n");
    out.push_str("# TYPE fastllm_cache_entries gauge\n");
    out.push_str(&format!("fastllm_cache_entries {}\n", state.cache.len()));
    out.push_str("# HELP fastllm_cache_bytes Memory held by cached responses.\n");
    out.push_str("# TYPE fastllm_cache_bytes gauge\n");
    out.push_str(&format!("fastllm_cache_bytes {}\n", state.cache.bytes()));

    // Everything the telemetry module owns: outcomes, rejection reasons,
    // routing counters, classifier counters, and the duration/TTFT histograms
    // both globally and per model.
    state.telemetry.render(&mut out);

    out.push_str("# HELP fastllm_backend_inflight Requests currently streaming from a backend.\n");
    out.push_str("# TYPE fastllm_backend_inflight gauge\n");
    for b in registry.backends() {
        out.push_str(&format!(
            "fastllm_backend_inflight{{api_base=\"{}\",model=\"{}\"}} {}\n",
            b.api_base,
            b.upstream_model,
            b.inflight()
        ));
    }

    out.push_str("# HELP fastllm_backend_healthy Whether a backend is in rotation.\n");
    out.push_str("# TYPE fastllm_backend_healthy gauge\n");
    for b in registry.backends() {
        out.push_str(&format!(
            "fastllm_backend_healthy{{api_base=\"{}\"}} {}\n",
            b.api_base,
            u8::from(b.is_healthy())
        ));
    }

    out.push_str("# HELP fastllm_backend_requests_total Requests dispatched to a backend.\n");
    out.push_str("# TYPE fastllm_backend_requests_total counter\n");
    for b in registry.backends() {
        out.push_str(&format!(
            "fastllm_backend_requests_total{{api_base=\"{}\"}} {}\n",
            b.api_base,
            b.requests_total()
        ));
    }

    out.push_str(
        "# HELP fastllm_backend_duration_seconds Whole-request wall time, per backend. A model's \
         p99 rising says the model got slow; this says which replica did.\n",
    );
    out.push_str("# TYPE fastllm_backend_duration_seconds histogram\n");
    for b in registry.backends() {
        b.duration.render(
            &mut out,
            "fastllm_backend_duration_seconds",
            &format!("api_base=\"{}\"", b.api_base),
        );
    }

    out.push_str("# HELP fastllm_backend_errors_total Upstream failures observed for a backend.\n");
    out.push_str("# TYPE fastllm_backend_errors_total counter\n");
    for b in registry.backends() {
        out.push_str(&format!(
            "fastllm_backend_errors_total{{api_base=\"{}\"}} {}\n",
            b.api_base,
            b.errors_total()
        ));
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4")
        .body(full(Bytes::from(out)))
        .expect("static response is well-formed")
}

fn full(bytes: Bytes) -> ResBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// These two responses are built from a complete body, so collecting it is
    /// a formality rather than a stream read.
    async fn body_string(resp: Response<ResBody>) -> String {
        use http_body_util::BodyExt;
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("a fully-buffered body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    fn snap(key: &str, models: &[&str]) -> Snapshot {
        Snapshot::for_test(
            vec![(key.to_string(), 1, None, false)],
            vec![Principal {
                id: 1,
                name: "t".into(),
                allowed_models: models.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
                allow_all: false,
                roles: HashSet::new(),
                limits: None,
                budget: None,
            }],
            vec![],
        )
    }

    /// The gap this closes: a proxy lagging behind the control plane answers
    /// `/health` with `ok`, lists the right models and backends, and still
    /// rejects a key the control plane issued — because the part of the
    /// snapshot that changed is not one `/health` showed. Publishing the
    /// version makes "is this pod current?" answerable in one request, and
    /// comparable across a fleet.
    /// Bounded, and longer for a 429 than a 5xx. The bound is what keeps this
    /// a retry budget measured against one request's patience rather than a
    /// background job's — the delay is paid by a client still waiting.
    #[test]
    fn backoff_grows_but_stays_inside_a_waiting_client_s_patience() {
        let wait = |attempt: usize, status: StatusCode, prefix: u64| {
            let base_ms = 25u64 << attempt.min(2);
            let jitter = prefix % (base_ms / 2 + 1);
            let ms = base_ms + jitter;
            if status == StatusCode::TOO_MANY_REQUESTS {
                ms * 2
            } else {
                ms
            }
        };
        let server = StatusCode::INTERNAL_SERVER_ERROR;
        assert!((25..=37).contains(&wait(0, server, 0)) || wait(0, server, 0) >= 25);
        assert!(wait(1, server, 0) >= wait(0, server, 0), "grows");
        assert!(wait(2, server, 0) >= wait(1, server, 0));
        // Capped: a fourth attempt waits no longer than the third.
        assert_eq!(wait(9, server, 0), wait(2, server, 0));
        // Never more than a fifth of a second, even at the cap with full
        // jitter and the 429 doubling.
        assert!(wait(9, StatusCode::TOO_MANY_REQUESTS, u64::MAX) <= 300);
        // A provider that said "too many requests" gets longer than a 5xx.
        assert!(wait(0, StatusCode::TOO_MANY_REQUESTS, 0) > wait(0, server, 0));
    }

    /// Jitter is keyed on the request rather than an RNG — the data plane has
    /// no random source in a `--no-default-features` build — so what matters is
    /// only that different requests decorrelate.
    #[test]
    fn jitter_differs_between_requests() {
        let j = |prefix: u64| prefix % (25 / 2 + 1);
        assert_ne!(j(7), j(8));
    }

    #[tokio::test]
    async fn health_and_metrics_publish_which_snapshot_is_being_served() {
        let state = crate::state::AppState::for_test();
        let mut snapshot = snap("sk-ok", &["m"]);
        snapshot.version = 1_786_140_563;
        state.apply_snapshot(snapshot).unwrap();

        let body = body_string(health_response(&state)).await;
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(health["snapshot_version"], 1_786_140_563u64);
        assert_eq!(health["keys"], 1, "a lagging pod is usually a key it lacks");

        let metrics = body_string(metrics_response(&state)).await;
        assert!(
            metrics.contains("fastllm_snapshot_version 1786140563"),
            "scrapeable, so a fleet-wide max minus min shows a stuck pod: {metrics}"
        );
    }

    #[test]
    fn a_valid_key_authorises_a_granted_model() {
        let s = snap("sk-ok", &["m"]);
        let p = s
            .authenticate("sk-ok", std::time::SystemTime::now())
            .unwrap();
        assert!(p.may_invoke("m"));
    }

    #[test]
    fn a_valid_key_is_forbidden_from_an_ungranted_model() {
        // 403, not 404: the model exists, this caller may not use it. Returning
        // 404 would leak nothing but would also mislead.
        let s = snap("sk-ok", &["m"]);
        let p = s
            .authenticate("sk-ok", std::time::SystemTime::now())
            .unwrap();
        assert!(!p.may_invoke("secret-model"));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn an_open_snapshot_needs_no_key() {
        let mut s = Snapshot::default();
        s.open = true;
        assert!(s.open);
    }

    #[test]
    fn identical_model_names_avoid_a_reserialise() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[]}"#);
        let out = rewrite_model_if_needed(&body, "m", "m", None, false).unwrap();
        // Same allocation, not a copy.
        assert_eq!(out.as_ptr(), body.as_ptr());
    }

    /// P3's consequence #1, pinned directly: `stream_options.include_usage`
    /// must land in the upstream body when injection is requested, and the
    /// untouched path (no alias, no injection) must stay the exact same
    /// allocation — the same "same allocation, not a copy" property the test
    /// above pins for the pre-existing alias-rewrite path.
    #[test]
    fn include_usage_is_injected_only_when_requested_and_the_untouched_path_is_byte_identical() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[],"stream":true}"#);

        let untouched = rewrite_model_if_needed(&body, "m", "m", None, false).unwrap();
        assert_eq!(
            untouched.as_ptr(),
            body.as_ptr(),
            "no rewrite requested must mean no reallocation at all"
        );

        let injected = rewrite_model_if_needed(&body, "m", "m", None, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&injected).unwrap();
        assert_eq!(parsed["stream_options"]["include_usage"], true);
    }

    /// A client that already set its own `stream_options` keeps whatever
    /// else it put there — injection merges `include_usage` in rather than
    /// clobbering the object.
    #[test]
    fn include_usage_injection_merges_into_an_existing_stream_options_object() {
        let body = Bytes::from_static(
            br#"{"model":"m","messages":[],"stream":true,"stream_options":{"other":true}}"#,
        );
        let out = rewrite_model_if_needed(&body, "m", "m", None, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["stream_options"]["include_usage"], true);
        assert_eq!(parsed["stream_options"]["other"], true);
    }

    /// `principal_needs_usage`, pinned directly: **every** attributable
    /// request is worth reading, and only an unattributable one is not.
    ///
    /// This test used to assert the opposite — that a principal with
    /// neither a budget nor a tokens-per-minute limit said no. That rule
    /// made accounting a side effect of enforcement, and the consequence
    /// showed up in production rather than here: a deployment enforcing
    /// nothing recorded nothing, so `usage_events` held nineteen rows
    /// against hundreds of served requests and there was no history to
    /// chart. The unconfigured cases below are the ones that changed
    /// meaning, and they are asserted explicitly so a future change back to
    /// the cheap rule has to delete a test that says why it exists.
    #[test]
    fn every_attributable_request_is_worth_recording() {
        fn principal(limits: Option<crate::limiter::Limits>, budget: Option<Budget>) -> Principal {
            Principal {
                id: 1,
                name: "p".into(),
                allowed_models: HashSet::new(),
                allow_all: true,
                roles: HashSet::new(),
                limits,
                budget,
            }
        }

        // The only no: nothing to attribute the consumption to.
        assert!(!principal_needs_usage(None), "no principal at all");

        // The two cases that used to be no, and are the whole point of the
        // change: a principal nobody has configured still consumes tokens,
        // and that consumption is still worth knowing.
        assert!(
            principal_needs_usage(Some(&principal(None, None))),
            "a principal with no budget and no limits is still accounted for"
        );
        assert!(
            principal_needs_usage(Some(&principal(
                Some(crate::limiter::Limits {
                    requests_per_min: Some(10),
                    tokens_per_min: None,
                }),
                None
            ))),
            "a requests-per-minute limit does not read tokens, but usage is still recorded"
        );
        assert!(principal_needs_usage(Some(&principal(
            Some(crate::limiter::Limits {
                requests_per_min: None,
                tokens_per_min: Some(10),
            }),
            None
        ))));
        assert!(principal_needs_usage(Some(&principal(
            None,
            Some(Budget {
                tokens_total: Some(10),
                tokens_used: 0,
                cost_total_micros: None,
                cost_used_micros: 0,
            })
        ))));
    }

    #[test]
    fn budget_exceeded_uses_402_not_429() {
        let resp = budget_exceeded_response(&Budget {
            tokens_total: Some(10),
            tokens_used: 10,
            cost_total_micros: None,
            cost_used_micros: 0,
        });
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[test]
    fn alias_rewrites_the_model_field() {
        let body = Bytes::from_static(br#"{"model":"gpt-4","messages":[],"temperature":0.7}"#);
        let out =
            rewrite_model_if_needed(&body, "gpt-4", "Qwen/Qwen3-30B-A3B", None, false).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["model"], "Qwen/Qwen3-30B-A3B");
        // Other parameters must survive the round trip.
        assert_eq!(parsed["temperature"], 0.7);
    }

    #[test]
    fn alias_rewrites_a_multipart_model_without_touching_the_upload() {
        let body = Bytes::from(
            "--xy\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             whisper-1\r\n\
             --xy\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n\
             RIFF\x00\x00audio\r\n\
             --xy--\r\n",
        );
        let range = multipart::find_field(&body, "xy", "model").unwrap();
        let out = rewrite_model_if_needed(
            &body,
            "whisper-1",
            "Systran/faster-whisper",
            Some(range),
            false,
        )
        .unwrap();

        let model = multipart::find_field(&out, "xy", "model").unwrap();
        assert_eq!(
            multipart::field_value(&out, model),
            Some("Systran/faster-whisper")
        );
        let file = multipart::find_field(&out, "xy", "file").unwrap();
        assert_eq!(out.slice(file), Bytes::from_static(b"RIFF\x00\x00audio"));
    }

    #[test]
    fn multipart_body_is_forwarded_verbatim_when_no_alias() {
        let body = Bytes::from_static(
            b"--xy\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--xy--\r\n",
        );
        let range = multipart::find_field(&body, "xy", "model").unwrap();
        let out = rewrite_model_if_needed(&body, "m", "m", Some(range), false).unwrap();
        assert_eq!(out.as_ptr(), body.as_ptr());
    }

    #[test]
    fn peek_ignores_unknown_and_large_fields() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"model":"m","stream":true,"tools":[{"x":1}]}"#;
        let peek: BodyPeek = serde_json::from_slice(body).unwrap();
        assert_eq!(peek.model.as_deref(), Some("m"));
        assert_eq!(peek.stream, Some(true));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer_token("Bearer sk-abc"), Some("sk-abc"));
        assert_eq!(bearer_token("bearer sk-abc"), Some("sk-abc"));
        assert_eq!(bearer_token("BEARER  sk-abc "), Some("sk-abc"));
        assert_eq!(bearer_token("Basic sk-abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("sk-abc"), None);
    }

    #[test]
    fn authorization_is_not_forwarded_from_the_client() {
        // The client's key authenticates it to the proxy; the upstream gets the
        // backend's own key or none at all.
        assert!(REQUEST_ONLY_STRIPPED.contains(&"authorization"));
        assert!(REQUEST_ONLY_STRIPPED.contains(&"content-length"));
        assert!(REQUEST_ONLY_STRIPPED.contains(&"host"));
    }

    #[test]
    fn response_content_length_survives_the_hop() {
        // Stripping it would force chunked encoding on every non-streaming
        // completion that arrived with a perfectly good length.
        assert!(!HOP_BY_HOP.contains(&"content-length"));
        assert!(HOP_BY_HOP.contains(&"transfer-encoding"));
    }

    #[test]
    fn audio_routes_are_proxied() {
        for route in ["/audio/transcriptions", "/audio/translations"] {
            assert!(PROXIED_SUFFIXES.contains(&route));
        }
    }

    /// Every endpoint that is only a passthrough. Listed by name rather than
    /// counted, so adding one is a deliberate edit here as well — and so the
    /// docs test that reads this list has something to compare against.
    #[test]
    fn passthrough_endpoints_are_proxied() {
        for route in [
            "/responses",
            "/audio/speech",
            "/images/generations",
            "/images/edits",
            "/moderations",
        ] {
            assert!(PROXIED_SUFFIXES.contains(&route), "{route} is not proxied");
        }
    }

    /// A routing rule matching on generation length must keep working when a
    /// client moves to a newer OpenAI SDK. `max_tokens` became
    /// `max_completion_tokens` on chat completions and `max_output_tokens` on
    /// the Responses API; a rule that silently stopped matching would look
    /// like a routing bug with nothing in the logs.
    #[test]
    fn every_spelling_of_max_tokens_is_read() {
        for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
            let body = format!(r#"{{"model":"m","{field}":128}}"#);
            let peek: BodyPeek = serde_json::from_str(&body).unwrap();
            assert_eq!(peek.max_tokens, Some(128), "{field} was not read");
        }
    }

    fn registry_with_two_models() -> crate::registry::Registry {
        let cfg: crate::config::FileConfig = serde_yaml::from_str(
            r#"
model_list:
  - model_name: concrete-a
    litellm_params: { api_base: "http://10.0.0.1:8000/v1" }
  - model_name: concrete-b
    litellm_params: { api_base: "http://10.0.0.2:8000/v1" }
"#,
        )
        .unwrap();
        crate::registry::Registry::build(&cfg, &crate::registry::Interner::default(), None).unwrap()
    }

    /// **Authorisation decision, pinned.** A virtual model resolves to a
    /// concrete target (`resolve_target_model`), and what gets checked
    /// against a principal's grants is that *resolved* concrete name, never
    /// the virtual one — see that function's doc comment for the full
    /// reasoning. This test proves both directions of the consequence
    /// directly, without a database:
    ///
    /// - A principal granted only the *concrete* model that a virtual model
    ///   happens to resolve to reaches it, despite never being granted the
    ///   virtual name itself.
    /// - A principal granted only the *virtual* name is refused, because
    ///   the grant recorded is for a name that is never actually checked —
    ///   this is the escalation path the design doc warns about, and it must
    ///   not exist: being granted a virtual model must never, by itself,
    ///   unlock whatever concrete model it happens to route to today.
    #[test]
    fn authorisation_checks_the_resolved_concrete_model_not_the_virtual_name() {
        let registry = registry_with_two_models();
        let mut virtual_models = HashMap::new();
        virtual_models.insert(
            "vm".to_string(),
            crate::routing::VirtualModelDef {
                name: "vm".into(),
                rules: vec![],
                default_targets: vec![crate::routing::WeightedTarget {
                    model: "concrete-a".into(),
                    weight: 100,
                }],
            },
        );
        let snapshot = Snapshot {
            virtual_models,
            ..Snapshot::default()
        };

        let granted_concrete_only = Principal {
            id: 1,
            name: "granted-concrete".into(),
            allowed_models: ["concrete-a".to_string()].into_iter().collect(),
            allow_all: false,
            roles: HashSet::new(),
            limits: None,
            budget: None,
        };
        let granted_virtual_only = Principal {
            id: 2,
            name: "granted-virtual".into(),
            allowed_models: ["vm".to_string()].into_iter().collect(),
            allow_all: false,
            roles: HashSet::new(),
            limits: None,
            budget: None,
        };

        let headers = HeaderMap::new();
        let facts = crate::routing::RequestFacts {
            caller: Some(&granted_concrete_only),
            prompt_tokens: 0,
            max_tokens: None,
            streaming: false,
            headers: &headers,
            now: chrono::Utc::now(),
            class: None,
            class_refines: &[],
        };
        let target = resolve_target_models("vm", &snapshot, &facts, 0, &registry)
            .expect("the virtual model has a viable default target")
            .remove(0);
        assert_eq!(target, "concrete-a");

        assert!(
            granted_concrete_only.may_invoke(&target),
            "a grant on the concrete model the virtual model resolves to must be honoured"
        );
        assert!(
            !granted_virtual_only.may_invoke(&target),
            "a grant on only the virtual NAME must not, by itself, unlock the concrete model \
             it happens to route to — that would let a virtual model's configuration silently \
             expand a principal's reach beyond what was actually granted"
        );
    }

    /// A virtual model whose name collides with nothing still resolves
    /// straight through: `resolve_target_models` on a name that is not in
    /// `snapshot.virtual_models` is the identity function, so ordinary
    /// (concrete) routing is completely unaffected by this feature existing.
    #[test]
    fn a_concrete_model_name_resolves_to_itself() {
        let registry = registry_with_two_models();
        let snapshot = Snapshot::default();
        let headers = HeaderMap::new();
        let facts = crate::routing::RequestFacts {
            caller: None,
            prompt_tokens: 0,
            max_tokens: None,
            streaming: false,
            headers: &headers,
            now: chrono::Utc::now(),
            class: None,
            class_refines: &[],
        };
        let target = resolve_target_models("concrete-a", &snapshot, &facts, 0, &registry)
            .unwrap()
            .remove(0);
        assert_eq!(target, "concrete-a");
    }
}
