//! Measures what a proxy in front of a real vLLM actually costs the user:
//! time to first token, and the gaps between tokens after that.
//!
//! Deliberately not a saturation test. spark2 is one replica serving real
//! traffic; the question is added latency per stream, not how hard it can be
//! pushed.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn pct(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i] as f64 / 1000.0
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let url = a[1].clone();
    let key = std::env::var("KEY").unwrap_or_default();
    let model = std::env::var("MODEL").unwrap_or_else(|_| "qwen3-6-35b-a3b-nvfp4".into());
    let conns: usize = a.get(2).map(|s| s.parse().unwrap()).unwrap_or(4);
    let per_conn: usize = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(2);
    let max_tokens: usize = a.get(4).map(|s| s.parse().unwrap()).unwrap_or(400);

    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_nodelay(true);
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(conns * 2)
        .build(connector);

    let frames = Arc::new(AtomicU64::new(0));
    let toks = Arc::new(AtomicU64::new(0));
    let ttft = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let gaps = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));

    let wall = Instant::now();
    let mut tasks = Vec::new();
    for c in 0..conns {
        let (client, url, key, model) = (client.clone(), url.clone(), key.clone(), model.clone());
        let (frames, toks, ttft, gaps) = (
            Arc::clone(&frames),
            Arc::clone(&toks),
            Arc::clone(&ttft),
            Arc::clone(&gaps),
        );
        tasks.push(tokio::spawn(async move {
            for r in 0..per_conn {
                // Vary the prompt per stream so prefix affinity is not the
                // thing being measured here.
                let body = format!(
                    r#"{{"model":"{model}","stream":true,"max_tokens":{max_tokens},"messages":[{{"role":"user","content":"Write {} distinct short sentences about the number {}."}}]}}"#,
                    max_tokens / 12,
                    c * 100 + r
                );
                let mut req = Request::builder()
                    .method("POST")
                    .uri(&url)
                    .header("content-type", "application/json");
                if !key.is_empty() {
                    req = req.header("authorization", format!("Bearer {key}"));
                }
                let req = req.body(Full::new(Bytes::from(body))).unwrap();

                let start = Instant::now();
                let Ok(resp) = client.request(req).await else {
                    eprintln!("request failed");
                    continue;
                };
                if resp.status() != 200 {
                    eprintln!("HTTP {}", resp.status());
                    continue;
                }
                let mut body = resp.into_body();
                let mut prev: Option<Instant> = None;
                let mut local_gaps = Vec::new();
                while let Some(Ok(frame)) = body.frame().await {
                    let Some(d) = frame.data_ref() else { continue };
                    let now = Instant::now();
                    frames.fetch_add(1, Ordering::Relaxed);
                    // Each SSE event is one token for a streaming completion.
                    toks.fetch_add(
                        d.windows(6).filter(|w| *w == b"data: ").count() as u64,
                        Ordering::Relaxed,
                    );
                    match prev {
                        None => ttft.lock().unwrap().push(now.duration_since(start).as_nanos() as u64),
                        Some(p) => local_gaps.push(now.duration_since(p).as_nanos() as u64),
                    }
                    prev = Some(now);
                }
                gaps.lock().unwrap().extend(local_gaps);
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    let secs = wall.elapsed().as_secs_f64();

    let mut t = ttft.lock().unwrap().clone();
    let mut g = gaps.lock().unwrap().clone();
    t.sort_unstable();
    g.sort_unstable();
    println!(
        "  ttft_ms p50 {:>7.1} p90 {:>7.1} | inter-token_ms p50 {:>6.2} p99 {:>7.2} | frames {:>6} events {:>6} | {:>6.1} tok/s agg | {:.1}s",
        pct(&t, 0.50) / 1000.0,
        pct(&t, 0.90) / 1000.0,
        pct(&g, 0.50) / 1000.0,
        pct(&g, 0.99) / 1000.0,
        frames.load(Ordering::Relaxed),
        toks.load(Ordering::Relaxed),
        toks.load(Ordering::Relaxed) as f64 / secs,
        secs
    );
}
