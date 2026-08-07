//! Mock vLLM-shaped upstream: answers instantly and streams N SSE frames with
//! no think time, so that anything in front of it is the bottleneck.

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::net::TcpListener;

/// TCP connections accepted. The proxy's pool is working if this stays far
/// below the request count.
static CONNS: AtomicU64 = AtomicU64::new(0);
static REQS: AtomicU64 = AtomicU64::new(0);

const CHUNK: &[u8] = b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" token\"},\"finish_reason\":null}]}\n\n";

struct Sse {
    left: usize,
    /// SSE events packed into each body frame. Raising this keeps total bytes
    /// constant while cutting the frame count, which separates per-frame
    /// overhead from per-byte overhead.
    per_frame: usize,
}

impl Body for Sse {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        let n = self.per_frame.min(self.left);
        self.left -= n;
        let b = if self.left == 0 {
            let mut v = CHUNK.repeat(n.saturating_sub(1));
            v.extend_from_slice(b"data: [DONE]\n\n");
            Bytes::from(v)
        } else if n == 1 {
            Bytes::from_static(CHUNK)
        } else {
            Bytes::from(CHUNK.repeat(n))
        };
        Poll::Ready(Some(Ok(Frame::data(b))))
    }
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

async fn handle(
    req: Request<hyper::body::Incoming>,
    tokens: usize,
    per_frame: usize,
) -> Result<Response<BoxBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    // Drain the request body; a real server reads what it was sent.
    let _ = req.into_body().collect().await;

    let n = REQS.fetch_add(1, Ordering::Relaxed) + 1;

    if path.ends_with("/stats") {
        let b = Bytes::from(format!(
            "{{\"conns\":{},\"reqs\":{}}}",
            CONNS.load(Ordering::Relaxed),
            n
        ));
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(http_body_util::Full::new(b).map_err(|e| match e {}).boxed())
            .unwrap());
    }

    let close_every: u64 = std::env::var("CLOSE_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let closing = close_every > 0 && n.is_multiple_of(close_every);

    if path.ends_with("/models") {
        let b = Bytes::from_static(br#"{"object":"list","data":[]}"#);
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(http_body_util::Full::new(b).map_err(|e| match e {}).boxed())
            .unwrap());
    }

    if tokens == 0 {
        let b = Bytes::from_static(br#"{"id":"c","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"}}]}"#);
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("connection", if closing { "close" } else { "keep-alive" })
            .header("content-type", "application/json")
            .body(http_body_util::Full::new(b).map_err(|e| match e {}).boxed())
            .unwrap());
    }

    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream");
    if closing {
        b = b.header("connection", "close");
    }
    Ok(b.body(
        Sse {
            left: tokens,
            per_frame,
        }
        .boxed(),
    )
    .unwrap())
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8100".into())
        .parse()
        .unwrap();
    let tokens: usize = std::env::var("TOKENS")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .unwrap();
    let per_frame: usize = std::env::var("PER_FRAME")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    // Loopback by default — this is a benchmark tool and should not be
    // reachable by accident. `HOST=0.0.0.0` is what a cross-machine comparison
    // needs, where the gateways under test run on a cluster and the mock has to
    // be reachable from it.
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = TcpListener::bind((host.as_str(), port)).await.unwrap();
    eprintln!("upstream on {host}:{port}, {tokens} sse frames per response");
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        CONNS.fetch_add(1, Ordering::Relaxed);
        stream.set_nodelay(true).unwrap();
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |r| handle(r, tokens, per_frame)),
                )
                .await;
        });
    }
}
