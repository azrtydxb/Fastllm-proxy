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
use crate::registry::{Backend, BackendUid, InflightGuard};
use crate::state::AppState;

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

    if let Some(rejection) = check_auth(&req, &state) {
        return Ok(rejection);
    }

    if method == Method::GET && (path == "/v1/models" || path == "/models") {
        return Ok(models_response(&state));
    }

    let subpath = path.strip_prefix("/v1").unwrap_or(&path);
    if method == Method::POST && PROXIED_SUFFIXES.contains(&subpath) {
        let subpath = subpath.to_string();
        return Ok(proxy_request(req, state, subpath).await);
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
}

async fn proxy_request(
    req: Request<Incoming>,
    state: Arc<AppState>,
    subpath: String,
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

    let (model, model_field, streaming) = match multipart::boundary(content_type) {
        // Multipart: the audio routes. `model` is a form field, and the rest of
        // the body — the upload — must not be touched.
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
                Some(model) => (model.to_string(), Some(range), false),
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
            (model, None, peek.stream.unwrap_or(false))
        }
    };

    let registry = state.registry.load();
    let Some(pool) = registry.pool(&model) else {
        state.requests_failed.fetch_add(1, Ordering::Relaxed);
        let known = registry.model_names().join(", ");
        return error_response(
            StatusCode::NOT_FOUND,
            "model_not_found",
            &format!("model {model:?} is not served here; available: [{known}]"),
        );
    };
    let pool = Arc::clone(pool);

    let prefix = state.router.prefix_key(&collected);
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
            &model,
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
                    model = %model,
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
    let detail = last_error.unwrap_or_else(|| format!("no healthy backend for model {model:?}"));
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

/// Returns a rejection response when the request may not proceed.
fn check_auth(req: &Request<Incoming>, state: &AppState) -> Option<Response<ResBody>> {
    let expected = state.master_key.as_ref()?;
    let presented = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);

    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => None,
        _ => Some(error_response(
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

/// Comparison that does not short-circuit on the first differing byte, so a
/// wrong key cannot be recovered one byte at a time. The length itself is not
/// hidden — for a bearer token that is not the secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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

/// OpenAI-shaped model list, aggregated over every pool.
fn models_response(state: &AppState) -> Response<ResBody> {
    let registry = state.registry.load();
    let data: Vec<serde_json::Value> = registry
        .model_names()
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
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
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
}
