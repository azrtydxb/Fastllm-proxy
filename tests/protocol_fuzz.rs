//! Malformed input into the protocol translators, which must never panic.
//!
//! # Why this is the place to fuzz
//!
//! Everything else this proxy parses is either its own configuration or a
//! request from an authenticated caller. The translators are different: they
//! parse **a third party's JSON**, on the response path, after a request has
//! already been billed and while bytes may already be moving to the client. A
//! provider that ships a bad build, truncates a stream mid-frame, or changes a
//! field's type is not a hypothetical — and `panic!` inside a connection task
//! takes that request down with a message the caller cannot act on.
//!
//! So the property under test is deliberately weak and total: **for any bytes
//! at all, translation returns or errors — it does not unwind.** Nothing here
//! asserts the output is *correct* for garbage input; that would be inventing
//! a specification for undefined data. Correctness on well-formed input is
//! `src/protocol/tests.rs`'s job.
//!
//! # Why not cargo-fuzz
//!
//! libFuzzer needs a nightly toolchain and a separate crate that CI would have
//! to build and run on a schedule — which in practice means it runs never. This
//! is structure-aware mutation over realistic seeds with a fixed PRNG seed, so
//! it runs in `cargo test` on every commit, costs about a second, and any
//! failure reproduces exactly from the printed iteration number.

use fastllm_proxy::protocol::{translate_request, translate_response, Protocol, ResponseContext};

/// Realistic payloads to mutate: the shapes these translators see in
/// production. Mutating from valid input finds far more than random bytes,
/// which almost always fail at the first `{`.
fn seeds() -> Vec<&'static [u8]> {
    vec![
        // An OpenAI request, the input side of translation.
        br#"{"model":"claude","messages":[{"role":"user","content":"hi"}],"max_tokens":64}"#,
        br#"{"model":"g","messages":[{"role":"system","content":"be brief"},{"role":"user","content":[{"type":"text","text":"x"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}]}],"stream":true}"#,
        br#"{"model":"c","messages":[{"role":"user","content":"go"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"a":{"type":"string"}}}}}],"tool_choice":"auto"}"#,
        // An Anthropic response.
        br#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#,
        // An Anthropic tool-use response.
        br#"{"id":"msg_2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"f","input":{"a":"b"}}],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":9}}"#,
        // A Gemini response.
        br#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}}"#,
        // A Gemini function call.
        br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{"a":1}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#,
    ]
}

/// SSE bodies, fed to the streaming translator a chunk at a time.
fn stream_seeds() -> Vec<&'static [u8]> {
    vec![
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"f\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"b\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}\n\n",
    ]
}

/// xorshift64*, so a failing iteration reproduces exactly.
///
/// Hand-rolled because this crate has no RNG in a `--no-default-features`
/// build and a fuzz harness is not a reason to add one.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// One mutation, chosen to hit the failure modes a real provider produces:
/// a truncated stream, a field that changed type, a number where a string was,
/// nesting, and bytes that are not UTF-8 at all.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut v = input.to_vec();
    if v.is_empty() {
        return v;
    }
    match rng.below(10) {
        // Truncation — by far the most common real failure: a connection cut
        // mid-frame leaves a partial JSON object.
        0 | 1 => {
            let at = rng.below(v.len());
            v.truncate(at);
        }
        // Single byte flipped: turns `"` into something else, breaks a number.
        2 => {
            let at = rng.below(v.len());
            v[at] ^= 1 << rng.below(8);
        }
        // Delete a span.
        3 => {
            let at = rng.below(v.len());
            let len = rng.below(v.len() - at).min(16);
            v.drain(at..at + len);
        }
        // Duplicate a span — repeated keys, unbalanced brackets.
        4 => {
            let at = rng.below(v.len());
            let len = rng.below(v.len() - at).min(16);
            let span = v[at..at + len].to_vec();
            v.splice(at..at, span);
        }
        // Splice in a token that changes a value's type.
        5 => {
            let at = rng.below(v.len());
            let token: &[u8] = match rng.below(6) {
                0 => b"null",
                1 => b"[]",
                2 => b"{}",
                3 => b"-1e999",
                4 => b"18446744073709551616",
                _ => b"\"\"",
            };
            v.splice(at..at, token.iter().copied());
        }
        // Bytes that are not valid UTF-8, which the SSE decoder must not hand
        // to `from_utf8` mid-sequence.
        6 => {
            let at = rng.below(v.len());
            v.splice(at..at, [0xF0, 0x9F, 0x92]);
        }
        // Control characters inside what is probably a string.
        7 => {
            let at = rng.below(v.len());
            v.splice(at..at, [0x00, 0x0A, 0x1B]);
        }
        // Deep nesting, to meet serde's recursion limit rather than the stack.
        8 => {
            let depth = 32 + rng.below(160);
            let mut nested = vec![b'['; depth];
            nested.extend_from_slice(&v);
            nested.extend(std::iter::repeat_n(b']', depth));
            v = nested;
        }
        // Empty.
        _ => v.clear(),
    }
    v
}

fn ctx(streaming: bool) -> ResponseContext {
    ResponseContext {
        model: "m".to_string(),
        request_id: 7,
        streaming,
        include_usage: true,
    }
}

/// Every entry point that touches provider bytes, over mutated input.
///
/// Failures are collected rather than asserted one at a time: a fuzzer that
/// stops at the first crash hides how many distinct inputs break, and the
/// second one is usually the informative one.
#[test]
fn translating_untrusted_bytes_never_panics() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut failures: Vec<String> = Vec::new();
    let protocols = [Protocol::Anthropic, Protocol::Gemini, Protocol::OpenAi];

    for (s, seed) in seeds().iter().enumerate() {
        for i in 0..600 {
            let input = mutate(&mut rng, seed);
            for protocol in protocols {
                let case = format!(
                    "seed {s}, iteration {i}, {protocol:?}, {} bytes",
                    input.len()
                );

                let r = std::panic::catch_unwind(|| {
                    let _ = translate_request(protocol, &input, "upstream", Some(64));
                });
                if r.is_err() {
                    failures.push(format!("translate_request: {case}"));
                }

                let r = std::panic::catch_unwind(|| {
                    let _ = translate_response(protocol, &input, &ctx(false));
                });
                if r.is_err() {
                    failures.push(format!("translate_response: {case}"));
                }

                let r = std::panic::catch_unwind(|| {
                    let _ = ResponseContext::from_request(&input, "m".into(), 1);
                });
                if r.is_err() {
                    failures.push(format!("ResponseContext::from_request: {case}"));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} input(s) panicked a translator:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// The streaming path, fed in arbitrary chunk boundaries.
///
/// Split points matter as much as content here: the decoder buffers a partial
/// frame between calls, so a bug that only appears when a multi-byte character
/// or an `\n\n` separator straddles two chunks is invisible to a test that
/// pushes a whole body at once.
#[test]
fn streaming_translation_never_panics_on_any_chunking() {
    use fastllm_proxy::protocol::StreamTranslator;

    let mut rng = Rng(0x5EED_1234_ABCD_0002);
    let mut failures: Vec<String> = Vec::new();

    for (s, seed) in stream_seeds().iter().enumerate() {
        for i in 0..600 {
            let input = mutate(&mut rng, seed);
            // A chunk size of 1 is the worst case for a buffering decoder and
            // the one a real network can genuinely produce.
            let chunk = 1 + rng.below(24);
            for protocol in [Protocol::Anthropic, Protocol::Gemini] {
                let case = format!(
                    "seed {s}, iteration {i}, {protocol:?}, chunk {chunk}, {} bytes",
                    input.len()
                );
                let r = std::panic::catch_unwind(|| {
                    let Some(mut t) = StreamTranslator::new(protocol, ctx(true)) else {
                        return;
                    };
                    for piece in input.chunks(chunk) {
                        let _ = t.push(piece);
                    }
                    let _ = t.finish();
                    // Idempotent by contract — a provider that closes after its
                    // own terminal event and one that just stops both land here.
                    let _ = t.finish();
                });
                if r.is_err() {
                    failures.push(format!("StreamTranslator: {case}"));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} input(s) panicked the stream translator:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
