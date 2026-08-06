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
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use pin_project_lite::pin_project;
use serde::Deserialize;
use std::ops::Range;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{debug, warn};

use crate::multipart;
use crate::registry::{Backend, BackendUid, InflightGuard, Registry};
use crate::snapshot::{AuthError, Principal, Snapshot};
use crate::state::AppState;
use std::time::SystemTime;

pub type ResBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

/// Endpoints forwarded to a backend. Everything else is served locally.
const PROXIED_SUFFIXES: &[&str] = &[
    "/chat/completions",
    "/completions",
    "/embeddings",
    "/rerank",
    "/score",
    "/audio/transcriptions",
    "/audio/translations",
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
        Err(rejection) => return Ok(rejection),
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
    #[serde(default)]
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
fn resolve_target_model(
    requested_model: &str,
    snapshot: &Snapshot,
    principal: Option<&Principal>,
    body_len: usize,
    max_tokens: Option<u64>,
    prefix: u64,
    registry: &Registry,
) -> Result<String, Response<ResBody>> {
    let Some(vm) = snapshot.virtual_models.get(requested_model) else {
        return Ok(requested_model.to_string());
    };
    let prompt_tokens = crate::routing::estimate_prompt_tokens(body_len);
    vm.resolve(principal, prompt_tokens, max_tokens, prefix, registry)
        .ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                "model_not_found",
                &format!(
                    "virtual model {requested_model:?} has no viable target for this request; \
                     check its routing rules and defaults"
                ),
            )
        })
}

async fn proxy_request(
    req: Request<Incoming>,
    state: Arc<AppState>,
    subpath: String,
    principal: Option<&Principal>,
    snapshot: &Snapshot,
) -> Response<ResBody> {
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

    let target_model = match resolve_target_model(
        &requested_model,
        snapshot,
        principal,
        collected.len(),
        max_tokens,
        prefix,
        &registry,
    ) {
        Ok(m) => m,
        Err(rejection) => {
            state.requests_failed.fetch_add(1, Ordering::Relaxed);
            return rejection;
        }
    };

    let Some(pool) = registry.pool(&target_model) else {
        state.requests_failed.fetch_add(1, Ordering::Relaxed);
        let known = registry.model_names().join(", ");
        return error_response(
            StatusCode::NOT_FOUND,
            "model_not_found",
            &format!("model {target_model:?} is not served here; available: [{known}]"),
        );
    };
    let pool = Arc::clone(pool);

    // Authorisation is a set lookup against the pre-flattened grant list, not
    // a walk of the RBAC graph, and costs nothing measurable. Checked against
    // `target_model` — see `resolve_target_model`'s doc comment for why a
    // virtual model's *resolved* concrete target is what gets authorised,
    // never the virtual name by itself.
    if let Some(principal) = principal {
        if !principal.may_invoke(&target_model) {
            state.requests_failed.fetch_add(1, Ordering::Relaxed);
            return error_response(
                StatusCode::FORBIDDEN,
                "model_access_denied",
                &format!("key is not permitted to use model {target_model:?}"),
            );
        }
    }

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
            if let crate::limiter::Decision::Exceeded { retry_after } =
                state
                    .limiter
                    .check(principal.id, limits, token_cost, std::time::Instant::now())
            {
                state.requests_failed.fetch_add(1, Ordering::Relaxed);
                return rate_limited_response(retry_after);
            }
        }
    }

    let mut tried: Vec<BackendUid> = Vec::new();
    let mut last_error: Option<String> = None;

    // Retries are safe here only because nothing has been written to the client
    // yet. Once a single upstream frame is forwarded the response is committed
    // and a failure mid-stream propagates as-is.
    for attempt in 0..=state.max_retries {
        let Some(backend) = state.router.pick(&pool, prefix, &tried) else {
            break;
        };
        tried.push(backend.uid);

        let upstream_body = match rewrite_model_if_needed(
            &collected,
            &requested_model,
            &backend.upstream_model,
            model_field.clone(),
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

        let upstream_req =
            match build_upstream_request(&parts.headers, &backend, &subpath, upstream_body) {
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
        let result = match tokio::time::timeout(state.upstream_headers_timeout, dispatch).await {
            Ok(r) => r,
            Err(_) => {
                backend.note_error();
                last_error = Some(format!(
                    "upstream {} did not send headers within {:?}",
                    backend.api_base, state.upstream_headers_timeout
                ));
                warn!(backend = %backend.api_base, "upstream headers timeout");
                continue;
            }
        };

        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error() {
                    backend.note_error();
                    // A 5xx before any bytes were forwarded is worth another
                    // node — but only if there *is* another node. Retrying into
                    // an empty candidate set would discard this response and
                    // answer with a synthetic 502, throwing away the upstream's
                    // own diagnostics; on a single-node pool that is every 5xx.
                    // 4xx is the client's fault and retrying just multiplies it.
                    if attempt < state.max_retries && state.router.has_candidate(&pool, &tried) {
                        last_error =
                            Some(format!("upstream {} returned {}", backend.api_base, status));
                        debug!(backend = %backend.api_base, %status, "retrying on another backend");
                        continue;
                    }
                }
                if status.is_client_error() || status.is_server_error() {
                    state.requests_failed.fetch_add(1, Ordering::Relaxed);
                } else {
                    state.requests_ok.fetch_add(1, Ordering::Relaxed);
                }
                debug!(
                    backend = %backend.api_base,
                    requested_model = %requested_model,
                    model = %target_model,
                    stream = streaming,
                    %status,
                    "proxied"
                );
                return finish_response(resp, guard);
            }
            Err(e) => {
                backend.note_error();
                last_error = Some(format!("upstream {} unreachable: {e}", backend.api_base));
                warn!(backend = %backend.api_base, error = %e, "upstream request failed");
                continue;
            }
        }
    }

    state.requests_failed.fetch_add(1, Ordering::Relaxed);
    let detail =
        last_error.unwrap_or_else(|| format!("no healthy backend for model {target_model:?}"));
    error_response(StatusCode::BAD_GATEWAY, "upstream_unavailable", &detail)
}

/// Rebuild the body with a different `model` value, or hand back the original
/// bytes when the names already match.
///
/// The common case is a straight `Bytes` clone (a refcount bump). Only aliases
/// pay for a rewrite — and for multipart, `model_field` is the already-located
/// range of the field value, so even that is a splice rather than a re-encode
/// of the upload.
fn rewrite_model_if_needed(
    body: &Bytes,
    requested: &str,
    upstream: &str,
    model_field: Option<Range<usize>>,
) -> Result<Bytes, serde_json::Error> {
    if requested == upstream {
        return Ok(body.clone());
    }
    if let Some(range) = model_field {
        return Ok(multipart::replace_range(body, range, upstream));
    }
    let mut value: serde_json::Value = serde_json::from_slice(body)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "model".into(),
            serde_json::Value::String(upstream.to_string()),
        );
    }
    Ok(Bytes::from(serde_json::to_vec(&value)?))
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
    if let Some(auth) = &backend.auth {
        headers.insert(HeaderName::from_static("authorization"), auth.clone());
    }

    Ok(builder.body(Full::new(body))?)
}

/// Hand the upstream response to the client, keeping the in-flight guard alive
/// for as long as the body is still streaming.
fn finish_response(resp: Response<UpstreamBody>, guard: InflightGuard) -> Response<ResBody> {
    let (mut parts, body) = resp.into_parts();
    // Hop-by-hop only: the response body is passed through byte for byte, so
    // an upstream `content-length` remains correct and is worth keeping.
    for name in HOP_BY_HOP {
        parts.headers.remove(*name);
    }
    Response::from_parts(parts, TrackedBody::new(body, guard).boxed())
}

pin_project! {
    /// Passthrough body that releases an [`InflightGuard`] when the stream ends.
    ///
    /// Deliberately not batching: merging frames that have already arrived was
    /// measured at a 1.000 merge ratio both against the pooled client and
    /// against the owned connection that replaced it, and against a real vLLM,
    /// whose tokens arrive tens of milliseconds apart. There is never a second
    /// frame waiting, so a coalescing buffer here only adds a poll and a branch.
    struct TrackedBody {
        #[pin]
        inner: UpstreamBody,
        guard: Option<InflightGuard>,
    }
}

impl TrackedBody {
    fn new(inner: UpstreamBody, guard: InflightGuard) -> Self {
        Self {
            inner,
            guard: Some(guard),
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
            "models": registry.model_names(),
            "backends": backends,
        })
        .to_string(),
    )
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
            }],
            vec![],
        )
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
        let out = rewrite_model_if_needed(&body, "m", "m", None).unwrap();
        // Same allocation, not a copy.
        assert_eq!(out.as_ptr(), body.as_ptr());
    }

    #[test]
    fn alias_rewrites_the_model_field() {
        let body = Bytes::from_static(br#"{"model":"gpt-4","messages":[],"temperature":0.7}"#);
        let out = rewrite_model_if_needed(&body, "gpt-4", "Qwen/Qwen3-30B-A3B", None).unwrap();
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
        let out =
            rewrite_model_if_needed(&body, "whisper-1", "Systran/faster-whisper", Some(range))
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
        let out = rewrite_model_if_needed(&body, "m", "m", Some(range)).unwrap();
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
        };
        let granted_virtual_only = Principal {
            id: 2,
            name: "granted-virtual".into(),
            allowed_models: ["vm".to_string()].into_iter().collect(),
            allow_all: false,
            roles: HashSet::new(),
            limits: None,
        };

        let target = resolve_target_model(
            "vm",
            &snapshot,
            Some(&granted_concrete_only),
            0,
            None,
            0,
            &registry,
        )
        .expect("the virtual model has a viable default target");
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
    /// straight through: `resolve_target_model` on a name that is not in
    /// `snapshot.virtual_models` is the identity function, so ordinary
    /// (concrete) routing is completely unaffected by this feature existing.
    #[test]
    fn a_concrete_model_name_resolves_to_itself() {
        let registry = registry_with_two_models();
        let snapshot = Snapshot::default();
        let target =
            resolve_target_model("concrete-a", &snapshot, None, 0, None, 0, &registry).unwrap();
        assert_eq!(target, "concrete-a");
    }
}
