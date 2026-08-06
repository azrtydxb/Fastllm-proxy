//! Closed-loop load generator. Reports request throughput and time-to-first-byte,
//! which is the number the proxy actually controls.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Key order matters: the OpenAI Python/Node SDKs and any sorted serialiser
/// emit `messages` before `model`, hand-written clients usually the reverse.
fn body_of(bytes: usize, model_first: bool) -> Bytes {
    let pad = bytes.saturating_sub(140);
    let msgs = format!(
        r#""messages":[{{"role":"system","content":"{}"}},{{"role":"user","content":"hello"}}]"#,
        "x".repeat(pad)
    );
    let s = if model_first {
        format!(r#"{{"model":"m","stream":true,{msgs},"temperature":0.7}}"#)
    } else {
        format!(r#"{{{msgs},"model":"m","stream":true,"temperature":0.7}}"#)
    };
    Bytes::from(s)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:4000/v1/chat/completions".into());
    let conns: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(32);
    let secs: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(5);
    let body_bytes: usize = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(1024);

    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_nodelay(true);
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(conns * 2)
        .build(connector);

    let model_first = std::env::var("MODEL_FIRST")
        .map(|v| v == "1")
        .unwrap_or(false);
    let body = body_of(body_bytes, model_first);
    let done = Arc::new(AtomicU64::new(0));
    let bytes_out = Arc::new(AtomicU64::new(0));
    let ttfb = Arc::new(parking_lot_lite::Mutex::new(Vec::<u64>::with_capacity(
        1 << 20,
    )));

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut tasks = Vec::new();
    for _ in 0..conns {
        let client = client.clone();
        let url = url.clone();
        let body = body.clone();
        let done = Arc::clone(&done);
        let bytes_out = Arc::clone(&bytes_out);
        let ttfb = Arc::clone(&ttfb);
        tasks.push(tokio::spawn(async move {
            let mut local: Vec<u64> = Vec::new();
            while Instant::now() < deadline {
                let req = Request::builder()
                    .method("POST")
                    .uri(&url)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer sk-bench")
                    .body(Full::new(body.clone()))
                    .unwrap();
                let start = Instant::now();
                let Ok(resp) = client.request(req).await else {
                    continue;
                };
                let mut first = true;
                let mut body = resp.into_body();
                let mut n = 0u64;
                while let Some(frame) = body.frame().await {
                    let Ok(frame) = frame else { break };
                    if first {
                        local.push(start.elapsed().as_nanos() as u64);
                        first = false;
                    }
                    if let Some(d) = frame.data_ref() {
                        n += d.len() as u64;
                    }
                }
                bytes_out.fetch_add(n, Ordering::Relaxed);
                done.fetch_add(1, Ordering::Relaxed);
            }
            ttfb.lock().extend_from_slice(&local);
        }));
    }

    let wall = Instant::now();
    for t in tasks {
        let _ = t.await;
    }
    let elapsed = wall.elapsed().as_secs_f64();

    let mut samples = ttfb.lock().clone();
    samples.sort_unstable();
    let pct = |p: f64| -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let i = ((samples.len() as f64 - 1.0) * p).round() as usize;
        samples[i] as f64 / 1000.0
    };
    let n = done.load(Ordering::Relaxed);
    println!(
        "req/s {:>9.0}   ttfb_us p50 {:>8.1}  p90 {:>8.1}  p99 {:>9.1}   resp_MiB/s {:>7.1}   n {}",
        n as f64 / elapsed,
        pct(0.50),
        pct(0.90),
        pct(0.99),
        bytes_out.load(Ordering::Relaxed) as f64 / elapsed / (1024.0 * 1024.0),
        n
    );
}

/// Tiny stand-in so the harness needs no extra dependency.
mod parking_lot_lite {
    pub struct Mutex<T>(std::sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub fn new(t: T) -> Self {
            Self(std::sync::Mutex::new(t))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
    }
}
