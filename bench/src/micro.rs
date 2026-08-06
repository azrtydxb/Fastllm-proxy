//! Microbenchmarks for the fixed per-request work on the proxy's hot path,
//! replicating exactly what proxy.rs does today.

use bytes::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request};
use std::time::Instant;

fn bench<F: FnMut()>(name: &str, iters: u32, mut f: F) {
    for _ in 0..iters / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    println!("  {name:<46} {ns:>9.0} ns");
}

#[derive(serde::Deserialize)]
struct BodyPeek {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: Option<bool>,
}

fn body_of(bytes: usize) -> Bytes {
    let pad = bytes.saturating_sub(120);
    Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "m", "stream": true,
            "messages": [{"role":"system","content":"x".repeat(pad)},{"role":"user","content":"hi"}],
            "temperature": 0.7
        }))
        .unwrap(),
    )
}

fn client_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h.insert("authorization", HeaderValue::from_static("Bearer sk-bench"));
    h.insert("accept", HeaderValue::from_static("*/*"));
    h.insert("user-agent", HeaderValue::from_static("bench/1.0"));
    h.insert("accept-encoding", HeaderValue::from_static("gzip"));
    h
}

const HOP: &[&str] = &[
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

fn main() {
    let api_base = "http://10.24.11.13:8000/v1".to_string();
    let subpath = "/chat/completions";
    let key = "sk-upstream-secret-token";
    let headers = client_headers();

    println!("per-request fixed work (current implementation):");

    bench("format!(url) + Uri parse", 200_000, || {
        let url = format!("{api_base}{subpath}");
        let uri: hyper::Uri = url.parse().unwrap();
        std::hint::black_box(uri);
    });

    bench("HeaderValue::from_str(format!(Bearer ..))", 500_000, || {
        let v = HeaderValue::from_str(&format!("Bearer {key}")).unwrap();
        std::hint::black_box(v);
    });

    bench("copy client headers (5) with hop filter", 500_000, || {
        let mut out = HeaderMap::new();
        for (n, v) in headers.iter() {
            if HOP.contains(&n.as_str()) {
                continue;
            }
            out.insert(n.clone(), v.clone());
        }
        out.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        std::hint::black_box(out);
    });

    bench("full Request::builder + body", 200_000, || {
        let url = format!("{api_base}{subpath}");
        let mut b = Request::builder().method(Method::POST).uri(&url);
        let h = b.headers_mut().unwrap();
        for (n, v) in headers.iter() {
            if HOP.contains(&n.as_str()) {
                continue;
            }
            h.insert(n.clone(), v.clone());
        }
        h.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
        let r = b
            .body(http_body_util::Full::new(Bytes::from_static(b"{}")))
            .unwrap();
        std::hint::black_box(r);
    });

    for size in [1024usize, 8192, 65536] {
        let body = body_of(size);
        bench(
            &format!("serde_json BodyPeek parse ({size} B body)"),
            100_000,
            || {
                let p: BodyPeek = serde_json::from_slice(&body).unwrap();
                std::hint::black_box((p.model, p.stream));
            },
        );
    }

    for size in [1024usize, 65536] {
        let body = body_of(size);
        bench(
            &format!("fxhash prefix_key 2048B ({size} B body)"),
            500_000,
            || {
                std::hint::black_box(fxhash(&body[..2048.min(body.len())]));
            },
        );
    }

    bench("path.to_string() + subpath.to_string()", 1_000_000, || {
        let p = "/v1/chat/completions".to_string();
        let s = p.strip_prefix("/v1").unwrap_or(&p).to_string();
        std::hint::black_box((p, s));
    });
}

fn fxhash(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
    let mut hash: u64 = 0;
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        hash = (hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
    let mut tail: u64 = 0;
    for (i, b) in chunks.remainder().iter().enumerate() {
        tail |= (*b as u64) << (i * 8);
    }
    hash = (hash.rotate_left(5) ^ tail).wrapping_mul(SEED);
    hash ^= hash >> 32;
    hash = hash.wrapping_mul(SEED);
    hash ^ (hash >> 29)
}
