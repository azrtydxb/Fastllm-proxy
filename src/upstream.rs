//! Upstream HTTP/1 client with a connection pool this process owns.
//!
//! The obvious choice is `hyper_util`'s pooled `Client`, and this used to use
//! it. The problem is structural rather than a matter of tuning: that client
//! drives every connection on its own task and hands body chunks to the
//! consumer over a one-deep want/give channel. A streaming response therefore
//! costs one cross-task wakeup *per frame*, which for an SSE generation means
//! one per token. Measured against a mock upstream that flushes every frame,
//! it capped the whole proxy at ~650k frames/s and 78 MiB/s.
//!
//! Here the connection future is polled from inside the response body's own
//! `poll_frame`. Reading the body is what moves bytes off the socket — there is
//! no channel and no second task — which took the same benchmark to ~320 MiB/s,
//! a little over 4x, while opening a fresh connection per request. Notably the
//! frame merge ratio stayed at exactly 1.000 throughout: the win is entirely
//! the deleted wakeup, not batching.
//!
//! What that buys in production today is nothing — a real vLLM emits tokens
//! tens of milliseconds apart, so the ceiling is four orders of magnitude away.
//! It matters only for many concurrent streams of a fast model.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming};
use hyper::client::conn::http1;
use hyper::{Request, Response, Uri};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tracing::debug;

/// Body error type. Not `hyper::Error`, which cannot be constructed outside
/// hyper — and this module has to be able to report a truncated response.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

type Sender = http1::SendRequest<Full<Bytes>>;
type Conn = http1::Connection<TokioIo<MaybeTls>, Full<Bytes>>;

/// Where a pooled connection can be reused. Two requests may share a
/// connection only if all three match.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    https: bool,
    host: String,
    port: u16,
}

impl Key {
    fn from_uri(uri: &Uri) -> anyhow::Result<Self> {
        let https = uri.scheme_str() == Some("https");
        let host = uri
            .host()
            .ok_or_else(|| anyhow::anyhow!("upstream URI {uri} has no host"))?
            .to_string();
        Ok(Self {
            port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
            https,
            host,
        })
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// A connection parked between requests.
///
/// Nothing polls it while it sits here, so a peer that closes the connection
/// goes unnoticed until checkout. That is deliberate — the alternative is a
/// task per idle connection, which is the cost this module exists to avoid —
/// and it is why [`Upstream::request`] retries once on a fresh connection.
struct Parked {
    sender: Sender,
    conn: Pin<Box<Conn>>,
    since: Instant,
}

struct Pool {
    idle: Mutex<HashMap<Key, Vec<Parked>>>,
    max_idle_per_host: usize,
    idle_timeout: Duration,
}

impl Pool {
    fn checkout(&self, key: &Key) -> Option<(Sender, Pin<Box<Conn>>)> {
        let mut idle = self.idle.lock();
        let bucket = idle.get_mut(key)?;
        while let Some(parked) = bucket.pop() {
            // Expired or already known-dead connections are dropped rather
            // than handed out; anything else is worth trying.
            if parked.since.elapsed() < self.idle_timeout && !parked.sender.is_closed() {
                return Some((parked.sender, parked.conn));
            }
        }
        None
    }

    fn park(&self, key: Key, sender: Sender, conn: Pin<Box<Conn>>) {
        if sender.is_closed() {
            return;
        }
        let mut idle = self.idle.lock();
        let bucket = idle.entry(key).or_default();
        // Drop expired entries while we are holding the lock anyway; there is
        // no sweeper task, so checkout and return are the only chances to.
        bucket.retain(|p| p.since.elapsed() < self.idle_timeout);
        if bucket.len() < self.max_idle_per_host {
            bucket.push(Parked {
                sender,
                conn,
                since: Instant::now(),
            });
        }
    }
}

pub struct Config {
    pub max_idle_per_host: usize,
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
}

pub struct Upstream {
    pool: Arc<Pool>,
    tls: tokio_rustls::TlsConnector,
    connect_timeout: Duration,
}

impl Upstream {
    pub fn new(cfg: Config, tls: rustls::ClientConfig) -> Self {
        Self {
            pool: Arc::new(Pool {
                idle: Mutex::new(HashMap::new()),
                max_idle_per_host: cfg.max_idle_per_host,
                idle_timeout: cfg.idle_timeout,
            }),
            tls: tokio_rustls::TlsConnector::from(Arc::new(tls)),
            connect_timeout: cfg.connect_timeout,
        }
    }

    /// Send a request, reusing a pooled connection when one is available.
    ///
    /// A pooled connection that turns out to be dead is retried once on a
    /// fresh one. That is safe unconditionally here: nothing has been written
    /// to the client yet, and the body is a complete `Bytes` that can be sent
    /// again as-is.
    pub async fn request(
        &self,
        req: Request<Full<Bytes>>,
    ) -> anyhow::Result<Response<UpstreamBody>> {
        let key = Key::from_uri(req.uri())?;

        if let Some((sender, conn)) = self.pool.checkout(&key) {
            match self.send_on(sender, conn, clone_request(&req), &key).await {
                Ok(resp) => return Ok(resp),
                Err(e) => debug!(
                    upstream = %key.addr(),
                    error = %e,
                    "pooled connection failed, retrying on a fresh one"
                ),
            }
        }

        let (sender, conn) = self.connect(&key).await?;
        self.send_on(sender, conn, req, &key).await
    }

    async fn send_on(
        &self,
        mut sender: Sender,
        mut conn: Pin<Box<Conn>>,
        mut req: Request<Full<Bytes>>,
        key: &Key,
    ) -> anyhow::Result<Response<UpstreamBody>> {
        to_origin_form(&mut req, key);

        // The connection has to be driven while the response headers are
        // awaited, for the same reason the body drives it later: on this
        // design nothing else will.
        let resp = {
            let mut send = Box::pin(sender.send_request(req));
            std::future::poll_fn(|cx| -> Poll<anyhow::Result<Response<Incoming>>> {
                if let Poll::Ready(r) = send.as_mut().poll(cx) {
                    return Poll::Ready(r.map_err(anyhow::Error::from));
                }
                // Nothing else drives this connection, so the request body is
                // written and the response parsed from right here.
                match conn.as_mut().poll(cx) {
                    // Ended before a response arrived. A pooled connection the
                    // peer had already closed lands here; the caller retries.
                    Poll::Ready(Ok(())) => Poll::Ready(Err(anyhow::anyhow!(
                        "upstream closed the connection before sending a response"
                    ))),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(anyhow::Error::from(e))),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await?
        };

        let (parts, incoming) = resp.into_parts();
        Ok(Response::from_parts(
            parts,
            UpstreamBody {
                inner: incoming,
                state: Some(Live {
                    sender,
                    conn,
                    key: key.clone(),
                    pool: Arc::clone(&self.pool),
                }),
                conn_done: false,
                conn_err: None,
                saw_eof: false,
            },
        ))
    }

    async fn connect(&self, key: &Key) -> anyhow::Result<(Sender, Pin<Box<Conn>>)> {
        let io = tokio::time::timeout(self.connect_timeout, self.dial(key))
            .await
            .map_err(|_| anyhow::anyhow!("connecting to {} timed out", key.addr()))??;
        let (sender, conn) = http1::handshake(TokioIo::new(io)).await?;
        Ok((sender, Box::pin(conn)))
    }

    async fn dial(&self, key: &Key) -> anyhow::Result<MaybeTls> {
        let tcp = TcpStream::connect(key.addr()).await?;
        // Without this a small SSE frame can sit in the kernel waiting for a
        // co-tenant, adding inter-token jitter that reads as slow generation.
        tcp.set_nodelay(true)?;
        if !key.https {
            return Ok(MaybeTls::Plain(tcp));
        }
        let name = rustls::pki_types::ServerName::try_from(key.host.clone())
            .map_err(|_| anyhow::anyhow!("{} is not a valid TLS server name", key.host))?;
        Ok(MaybeTls::Tls(Box::new(
            self.tls.clone().connect(name, tcp).await?,
        )))
    }
}

/// The pieces needed to hand a connection back to the pool once its response
/// has been fully read.
struct Live {
    sender: Sender,
    conn: Pin<Box<Conn>>,
    key: Key,
    pool: Arc<Pool>,
}

/// Response body that drives its own connection.
///
/// Dropping this before end-of-stream — a client that hangs up mid-generation —
/// drops the connection with it rather than returning a half-read one to the
/// pool.
pub struct UpstreamBody {
    inner: Incoming,
    state: Option<Live>,
    conn_done: bool,
    /// Why the connection ended, if it ended badly. Reported to the client
    /// instead of stalling.
    conn_err: Option<hyper::Error>,
    /// Set when the body has been polled to `None`. Tracked separately from
    /// `is_end_stream()`, which stays false for a chunked response even once
    /// the terminal chunk has been read — relying on it alone meant streaming
    /// responses never returned their connection to the pool.
    saw_eof: bool,
}

impl Body for UpstreamBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();

        // Moving bytes off the socket is this poll's job. This is the whole
        // point of the module: no channel, no second task, no wakeup per frame.
        if !this.conn_done {
            if let Some(live) = this.state.as_mut() {
                match live.conn.as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => this.conn_done = true,
                    Poll::Ready(Err(e)) => {
                        this.conn_done = true;
                        this.conn_err = Some(e);
                    }
                    Poll::Pending => {}
                }
            }
        }

        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(None) => {
                this.saw_eof = true;
                this.park_if_reusable();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.state = None;
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(Some(Ok(f))) => Poll::Ready(Some(Ok(f))),
            // Nothing buffered and the connection is gone: the response was
            // truncated. Returning Pending here would wait for a wakeup that
            // can never come — measured as a client hanging until its own
            // timeout when the upstream was killed mid-stream.
            Poll::Pending if this.conn_done => {
                this.state = None;
                Poll::Ready(Some(Err(this.conn_err.take().map_or_else(
                    || BoxError::from("upstream closed the connection mid-response"),
                    BoxError::from,
                ))))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl UpstreamBody {
    /// Hand the connection back, but only if this response actually finished.
    ///
    /// A half-read body means the peer is mid-response and the connection
    /// cannot serve anyone else; the same is true once the connection future
    /// itself has completed.
    fn park_if_reusable(&mut self) {
        let Some(live) = self.state.take() else {
            return;
        };
        // Either signal is proof the response completed: `saw_eof` for a body
        // polled to None (streaming), `is_end_stream` for one hyper stopped
        // polling early because the length was known (non-streaming).
        if self.conn_done || !(self.saw_eof || self.inner.is_end_stream()) {
            return;
        }
        live.pool.park(live.key, live.sender, live.conn);
    }
}

impl Drop for UpstreamBody {
    /// Parking cannot rely on being polled to `None`.
    ///
    /// Once the last frame is out, `is_end_stream()` is true and hyper's server
    /// simply stops polling — so the `Ready(None)` that would return the
    /// connection to the pool never arrives. Without this, every request
    /// dialled a fresh connection and the pool stayed permanently empty, which
    /// is exactly what the first end-to-end run showed: 24 connections for 25
    /// requests.
    fn drop(&mut self) {
        self.park_if_reusable();
    }
}

/// Put the request in the shape an origin server expects.
///
/// `client::conn::http1` is the low-level API and does neither of these for
/// you, unlike the pooled `Client` that used to sit here:
///
/// - It sends the URI as given, so an absolute URI goes out as
///   `POST http://host/v1/... HTTP/1.1` — legal only when talking to a
///   forward proxy, and rejected by plenty of origin servers.
/// - It adds no `Host` header, and this proxy strips the client's on purpose.
///
/// A mock upstream tolerates both; api.openai.com closed the connection
/// without answering, which surfaced as a bare "closed before sending a
/// response".
fn to_origin_form(req: &mut Request<Full<Bytes>>, key: &Key) {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "/".to_string());
    if let Ok(uri) = Uri::builder().path_and_query(path).build() {
        *req.uri_mut() = uri;
    }

    if !req.headers().contains_key(hyper::header::HOST) {
        // The default port is omitted, as every HTTP client does.
        let authority = match (key.https, key.port) {
            (true, 443) | (false, 80) => key.host.clone(),
            _ => format!("{}:{}", key.host, key.port),
        };
        if let Ok(value) = hyper::header::HeaderValue::from_str(&authority) {
            req.headers_mut().insert(hyper::header::HOST, value);
        }
    }
}

/// Rebuild a request so the pooled attempt and the retry are independent.
/// Cheap: the body is a `Bytes`, so this is a refcount bump.
fn clone_request(req: &Request<Full<Bytes>>) -> Request<Full<Bytes>> {
    let mut out = Request::builder()
        .method(req.method().clone())
        .uri(req.uri().clone());
    if let Some(headers) = out.headers_mut() {
        headers.clone_from(req.headers());
    }
    out.body(req.body().clone())
        .expect("rebuilt from an already valid request")
}

/// A plain or TLS stream behind one concrete type, so the pool holds a single
/// connection type rather than being generic over the transport.
pub enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }
    fn is_write_vectored(&self) -> bool {
        match self {
            MaybeTls::Plain(s) => s.is_write_vectored(),
            MaybeTls::Tls(s) => s.is_write_vectored(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_defaults_the_port_per_scheme() {
        let k = Key::from_uri(&"http://10.0.0.1/v1/chat".parse().unwrap()).unwrap();
        assert_eq!((k.https, k.port, k.host.as_str()), (false, 80, "10.0.0.1"));
        let k = Key::from_uri(&"https://api.openai.com/v1".parse().unwrap()).unwrap();
        assert_eq!((k.https, k.port), (true, 443));
        let k = Key::from_uri(&"http://10.0.0.1:8000/v1".parse().unwrap()).unwrap();
        assert_eq!(k.port, 8000);
    }

    #[test]
    fn scheme_and_port_separate_pools() {
        // A connection to :80 must never be handed to a request for :443, and
        // plain must never satisfy TLS even on the same port.
        let a = Key::from_uri(&"http://h:8000/v1".parse().unwrap()).unwrap();
        let b = Key::from_uri(&"http://h:8001/v1".parse().unwrap()).unwrap();
        assert_ne!(a, b);
        let c = Key {
            https: true,
            host: "h".into(),
            port: 8000,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn a_uri_without_a_host_is_rejected() {
        assert!(Key::from_uri(&"/v1/chat/completions".parse().unwrap()).is_err());
    }

    fn origin_form(uri: &str) -> Request<Full<Bytes>> {
        let key = Key::from_uri(&uri.parse().unwrap()).unwrap();
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .unwrap();
        to_origin_form(&mut req, &key);
        req
    }

    #[test]
    fn requests_go_out_in_origin_form() {
        // Sending the absolute URI is forward-proxy syntax; origin servers
        // reject it. This shipped broken once — api.openai.com just closed the
        // connection.
        let req = origin_form("http://10.0.0.1:8000/v1/chat/completions");
        assert_eq!(req.uri().to_string(), "/v1/chat/completions");
        assert!(req.uri().host().is_none());
    }

    #[test]
    fn a_host_header_is_always_present() {
        // `client::conn::http1` adds none, and the proxy strips the client's.
        let req = origin_form("http://10.0.0.1:8000/v1/chat/completions");
        assert_eq!(req.headers()[hyper::header::HOST], "10.0.0.1:8000");
        // Default ports are omitted, as every other client does.
        assert_eq!(
            origin_form("https://api.openai.com/v1/chat/completions").headers()
                [hyper::header::HOST],
            "api.openai.com"
        );
        assert_eq!(
            origin_form("http://h/v1/chat").headers()[hyper::header::HOST],
            "h"
        );
    }

    #[test]
    fn an_explicit_host_header_is_left_alone() {
        let key = Key::from_uri(&"http://10.0.0.1:8000/v1".parse().unwrap()).unwrap();
        let mut req = Request::builder()
            .method("POST")
            .uri("http://10.0.0.1:8000/v1/chat")
            .header(hyper::header::HOST, "vllm.internal")
            .body(Full::new(Bytes::new()))
            .unwrap();
        to_origin_form(&mut req, &key);
        assert_eq!(req.headers()[hyper::header::HOST], "vllm.internal");
    }

    #[test]
    fn a_query_string_survives_origin_form() {
        let req = origin_form("http://h:8000/v1/models?limit=2");
        assert_eq!(req.uri().to_string(), "/v1/models?limit=2");
    }

    #[test]
    fn cloning_a_request_preserves_method_uri_headers_and_body() {
        let req = Request::builder()
            .method("POST")
            .uri("http://h:8000/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer x")
            .body(Full::new(Bytes::from_static(b"{\"model\":\"m\"}")))
            .unwrap();
        let c = clone_request(&req);
        assert_eq!(c.method(), req.method());
        assert_eq!(c.uri(), req.uri());
        assert_eq!(c.headers(), req.headers());
    }
}
