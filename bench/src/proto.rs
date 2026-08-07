//! Prototype for TODO item 1: own the upstream connection instead of using
//! hyper-util's pooled Client.
//!
//! The pooled client runs each connection in its own task and hands body
//! chunks over a 1-deep want/give channel, so every frame costs a cross-task
//! wakeup and a second frame is never already waiting (measured merge ratio
//! 1.000). This drives the connection future *inside* the response body's own
//! poll, so reading the body is what moves bytes off the socket — no channel,
//! no second task — and several chunks can come out of one read.
//!
//! Deliberately no connection pool: pooling changes connection setup cost, not
//! the per-frame cost this is testing. One upstream connection per request.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::client::conn::http1 as client_http1;
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::net::{TcpListener, TcpStream};

pub static FRAMES_IN: AtomicU64 = AtomicU64::new(0);
pub static FRAMES_OUT: AtomicU64 = AtomicU64::new(0);

type Conn = client_http1::Connection<TokioIo<TcpStream>, Full<Bytes>>;

const COALESCE_LIMIT: usize = 64 * 1024;

/// Response body that drives the upstream connection as a side effect of being
/// read, and merges every frame that read makes available.
struct InlineBody {
    conn: Pin<Box<Conn>>,
    conn_done: bool,
    inner: Incoming,
    ended: bool,
}

impl Body for InlineBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }

        // Moving bytes off the socket is this poll's job, not another task's.
        if !this.conn_done && this.conn.as_mut().poll(cx).is_ready() {
            this.conn_done = true;
        }

        let mut merged: Option<bytes::BytesMut> = None;
        let mut first: Option<Bytes> = None;
        let mut total = 0usize;

        loop {
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    FRAMES_IN.fetch_add(1, Ordering::Relaxed);
                    total += data.len();
                    match first {
                        None => first = Some(data),
                        Some(_) => {
                            let f = first.clone().unwrap();
                            merged
                                .get_or_insert_with(|| {
                                    let mut b = bytes::BytesMut::with_capacity(COALESCE_LIMIT);
                                    b.extend_from_slice(&f);
                                    b
                                })
                                .extend_from_slice(&data);
                        }
                    }
                    if total >= COALESCE_LIMIT {
                        break;
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    this.ended = true;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    this.ended = true;
                    break;
                }
                Poll::Pending => break,
            }
        }

        match (merged, first) {
            (Some(m), _) => {
                FRAMES_OUT.fetch_add(1, Ordering::Relaxed);
                Poll::Ready(Some(Ok(Frame::data(m.freeze()))))
            }
            (None, Some(f)) => {
                FRAMES_OUT.fetch_add(1, Ordering::Relaxed);
                Poll::Ready(Some(Ok(Frame::data(f))))
            }
            (None, None) if this.ended => Poll::Ready(None),
            (None, None) => Poll::Pending,
        }
    }
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

async fn handle(
    req: Request<Incoming>,
    upstream: String,
) -> Result<Response<BoxBody>, hyper::Error> {
    let body = req.into_body().collect().await?.to_bytes();

    let stream = TcpStream::connect(&upstream).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let (mut sender, conn) = client_http1::handshake(TokioIo::new(stream)).await?;
    let mut conn = Box::pin(conn);

    let out = Request::builder()
        .method("POST")
        .uri(format!("http://{upstream}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(Full::new(body))
        .unwrap();

    // Drive the connection while waiting for response headers, for the same
    // reason the body does: nothing else is going to.
    let mut send = Box::pin(sender.send_request(out));
    let resp = std::future::poll_fn(|cx| {
        if let Poll::Ready(r) = send.as_mut().poll(cx) {
            return Poll::Ready(r);
        }
        let _ = conn.as_mut().poll(cx);
        Poll::Pending
    })
    .await?;

    let (parts, incoming) = resp.into_parts();
    Ok(Response::from_parts(
        parts,
        InlineBody {
            conn,
            conn_done: false,
            inner: incoming,
            ended: false,
        }
        .boxed(),
    ))
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "4200".into())
        .parse()
        .unwrap();
    let upstream = std::env::var("UPSTREAM").unwrap_or_else(|_| "127.0.0.1:8100".into());
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    eprintln!("proto proxy on {port} -> {upstream}");

    tokio::spawn(async {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            t.tick().await;
            let (i, o) = (
                FRAMES_IN.load(Ordering::Relaxed),
                FRAMES_OUT.load(Ordering::Relaxed),
            );
            if o > 0 {
                eprintln!("merge ratio {:.3}  (in {i} out {o})", i as f64 / o as f64);
            }
        }
    });

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        stream.set_nodelay(true).unwrap();
        let up = upstream.clone();
        tokio::spawn(async move {
            let _ = server_http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |r| handle(r, up.clone())),
                )
                .await;
        });
    }
}
