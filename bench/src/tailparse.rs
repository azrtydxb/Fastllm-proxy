//! What every attributable request now pays for usage accounting.
//!
//! Usage recording used to be limited to principals with a budget or a
//! token-rate limit, so this cost fell on a minority of traffic. It is now
//! paid on every request with a principal, and a claim about a per-request
//! cost in this repo needs a number rather than a paragraph.
//!
//! Two things are measured, because they are the two the change added:
//!
//! 1. `TailBuffer::push` per frame — a memcpy into a ring, no parse. This is
//!    the part that scales with response size, so it is measured across
//!    frame counts rather than once.
//! 2. `extract_usage` once at end of stream — the parse. Measured on the
//!    three shapes it actually meets: a small non-streaming body, an SSE
//!    tail, and the case the recent fix added, a tail that is a *fragment*
//!    of a body far larger than the window (a 22 KB embeddings response).
//!
//! Run: `cargo run -p bench --release --bin tailparse`

use fastllm_proxy::tail_buffer::{TailBuffer, DEFAULT_CAPACITY};
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
    println!("  {name:<52} {ns:>9.0} ns");
}

fn sse_tail() -> Vec<u8> {
    let mut out = Vec::new();
    // Sixty delta frames, then the usage chunk and the sentinel — the shape
    // a streamed completion actually ends with.
    for i in 0..60 {
        out.extend_from_slice(
            format!(
                "data: {{\"id\":\"c\",\"choices\":[{{\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        b"data: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\
          \"completion_tokens\":34,\"total_tokens\":46}}\n\ndata: [DONE]\n\n",
    );
    out
}

fn json_body() -> Vec<u8> {
    br#"{"id":"c","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}"#.to_vec()
}

/// An embeddings response: a long float array, usage last. Deliberately far
/// larger than the window, so the tail is a fragment with no opening brace —
/// the case that used to yield nothing at all.
fn big_embeddings_body() -> Vec<u8> {
    let vector: String = (0..4000).map(|i| format!("{}.{:04},", i % 10, i)).collect();
    format!(
        r#"{{"object":"list","data":[{{"object":"embedding","index":0,"embedding":[{vector}0.0]}}],"model":"bge-m3","usage":{{"prompt_tokens":4,"total_tokens":4,"completion_tokens":0}}}}"#
    )
    .into_bytes()
}

fn main() {
    println!("tail buffer + usage extraction, per request\n");

    let frame = b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"token\"}}]}\n\n";
    bench("push one SSE frame (memcpy into the ring)", 2_000_000, || {
        let mut t = TailBuffer::new(DEFAULT_CAPACITY);
        t.push(std::hint::black_box(frame));
    });

    let sse = sse_tail();
    bench("push a whole 60-frame stream", 200_000, || {
        let mut t = TailBuffer::new(DEFAULT_CAPACITY);
        for chunk in sse.chunks(64) {
            t.push(std::hint::black_box(chunk));
        }
    });

    let json = json_body();
    bench("extract_usage: small non-streaming body", 500_000, || {
        let mut t = TailBuffer::new(DEFAULT_CAPACITY);
        t.push(&json);
        std::hint::black_box(t.extract_usage());
    });

    bench("extract_usage: SSE tail (60 frames)", 200_000, || {
        let mut t = TailBuffer::new(DEFAULT_CAPACITY);
        t.push(&sse);
        std::hint::black_box(t.extract_usage());
    });

    let big = big_embeddings_body();
    println!("  (embeddings fixture: {} bytes vs {DEFAULT_CAPACITY} window)", big.len());
    bench("extract_usage: 22 KB body, tail is a fragment", 200_000, || {
        let mut t = TailBuffer::new(DEFAULT_CAPACITY);
        t.push(&big);
        std::hint::black_box(t.extract_usage());
    });

    bench("extract_usage: tail carrying no usage at all", 200_000, || {
        let mut t = TailBuffer::new(DEFAULT_CAPACITY);
        t.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"nothing here\"}}]}\n\n");
        std::hint::black_box(t.extract_usage());
    });
}
