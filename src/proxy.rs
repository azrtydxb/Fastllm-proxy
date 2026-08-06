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

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use pin_project_lite::pin_project;
use serde::Deserialize;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{debug, warn};

use crate::registry::{Backend, BackendUid, InflightGuard};
use crate::state::AppState;

pub type ResBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

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

/// Headers that must not be copied between hops.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "authorization",
];

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
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                &format!("request body exceeds {} bytes", state.max_body_bytes),
            )
        }
    };

    let peek: BodyPeek = match serde_json::from_slice(&collected) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("request body is not valid JSON: {e}"),
            )
        }
    };

    let Some(model) = peek.model.filter(|m| !m.is_empty()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body is missing the 'model' field",
        );
    };

    let registry = state.registry.load();
    let Some(pool) = registry.pool(&model) else {
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

        let upstream_body =
            match rewrite_model_if_needed(&collected, &model, &backend.upstream_model) {
                Ok(b) => b,
                Err(e) => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("could not rewrite model name for alias: {e}"),
                    )
                }
            };

        let upstream_req =
            match build_upstream_request(&parts.headers, &backend, &subpath, upstream_body) {
                Ok(r) => r,
                Err(e) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        &format!("could not build upstream request: {e}"),
                    )
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
                // A 5xx before any bytes were forwarded is worth another node.
                // 4xx is the client's fault and retrying just multiplies it.
                if status.is_server_error() && attempt < state.max_retries {
                    backend.note_error();
                    last_error = Some(format!("upstream {} returned {}", backend.api_base, status));
                    debug!(backend = %backend.api_base, %status, "retrying on another backend");
                    continue;
                }
                if status.is_server_error() {
                    backend.note_error();
                }
                state.requests_ok.fetch_add(1, Ordering::Relaxed);
                debug!(
                    backend = %backend.api_base,
                    model = %model,
                    stream = peek.stream.unwrap_or(false),
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
/// pay for a parse and re-serialise.
fn rewrite_model_if_needed(
    body: &Bytes,
    requested: &str,
    upstream: &str,
) -> Result<Bytes, serde_json::Error> {
    if requested == upstream {
        return Ok(body.clone());
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
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    if let Some(key) = &backend.api_key {
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&format!("Bearer {key}"))?,
        );
    }

    Ok(builder.body(Full::new(body))?)
}

/// Hand the upstream response to the client, keeping the in-flight guard alive
/// for as long as the body is still streaming.
fn finish_response(resp: Response<Incoming>, guard: InflightGuard) -> Response<ResBody> {
    let (mut parts, body) = resp.into_parts();
    for name in HOP_BY_HOP {
        parts.headers.remove(*name);
    }
    Response::from_parts(parts, TrackedBody::new(body, guard).boxed())
}

pin_project! {
    /// Passthrough body that releases an [`InflightGuard`] when the stream ends.
    struct TrackedBody {
        #[pin]
        inner: Incoming,
        guard: Option<InflightGuard>,
    }
}

impl TrackedBody {
    fn new(inner: Incoming, guard: InflightGuard) -> Self {
        Self {
            inner,
            guard: Some(guard),
        }
    }
}

impl Body for TrackedBody {
    type Data = Bytes;
    type Error = hyper::Error;

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
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);

    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => None,
        _ => Some(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid bearer token",
        )),
    }
}

/// Length-independent comparison, so a wrong key cannot be probed byte by byte.
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

    out.push_str("# HELP fastllm_requests_total Requests proxied successfully.\n");
    out.push_str("# TYPE fastllm_requests_total counter\n");
    out.push_str(&format!(
        "fastllm_requests_total {}\n",
        state.requests_ok.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP fastllm_requests_failed_total Requests that exhausted every backend.\n");
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
        let out = rewrite_model_if_needed(&body, "m", "m").unwrap();
        // Same allocation, not a copy.
        assert_eq!(out.as_ptr(), body.as_ptr());
    }

    #[test]
    fn alias_rewrites_the_model_field() {
        let body = Bytes::from_static(br#"{"model":"gpt-4","messages":[],"temperature":0.7}"#);
        let out = rewrite_model_if_needed(&body, "gpt-4", "Qwen/Qwen3-30B-A3B").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["model"], "Qwen/Qwen3-30B-A3B");
        // Other parameters must survive the round trip.
        assert_eq!(parsed["temperature"], 0.7);
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
    fn authorization_is_not_forwarded_from_the_client() {
        // The client's key authenticates it to the proxy; the upstream gets the
        // backend's own key or none at all.
        assert!(HOP_BY_HOP.contains(&"authorization"));
        assert!(HOP_BY_HOP.contains(&"content-length"));
        assert!(HOP_BY_HOP.contains(&"host"));
    }
}
