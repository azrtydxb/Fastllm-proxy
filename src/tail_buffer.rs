//! P3's answer to "usage lives in the response body, but this proxy never
//! parses response bodies" (design doc, "P3 -- Usage accounting and
//! budgets").
//!
//! Every upstream frame still reaches the client untouched and in order —
//! nothing here sits in that path. What this keeps is a *second*, bounded
//! copy of only the last [`CAPACITY`] bytes forwarded, appended to as frames
//! go by (a memcpy, not a parse) and inspected exactly once, when the stream
//! ends. Usage always appears in the final SSE event or as a top-level field
//! of a non-streaming body, so the tail — not the whole response — is enough
//! to find it. This is the "one small parse per request, not per frame" the
//! design commits to.
//!
//! Bounded and fixed-capacity by construction: [`TailBuffer::push`] never
//! grows the buffer past `cap`, so an arbitrarily long generation costs this
//! module a constant amount of memory, not one proportional to the response.

use std::collections::VecDeque;

/// A few KB is comfortably more than one SSE usage chunk
/// (`{"id":...,"usage":{"prompt_tokens":N,"completion_tokens":N,...}}` is a
/// few hundred bytes) plus the `data: [DONE]\n\n` sentinel that typically
/// follows it, with headroom for a non-streaming body's `usage` object to
/// still be findable even if it is not the very last key serialised.
pub const DEFAULT_CAPACITY: usize = 8 * 1024;

pub struct TailBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

/// What the extractor was looking for: enough to bill and to advance a
/// budget or a token-rate bucket, nothing more — the same two fields
/// `crate::usage::UsageEvent` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageTokens {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl TailBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Append forwarded bytes, dropping the oldest ones so the buffer never
    /// exceeds `cap`. `VecDeque::push_back`/`pop_front` are both O(1)
    /// amortised, and the deque was reserved to `cap` up front, so a steady
    /// stream of frames costs a memcpy per frame, never a reallocation.
    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            // This one frame alone would fill (or overflow) the buffer —
            // whatever was in it before is now entirely out of the tail
            // window anyway, so just keep this frame's own last `cap` bytes.
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend(data.iter().copied());
    }

    /// The one parse per request: look at the accumulated tail and try to
    /// find a `usage` object in it, once, at end of stream. Never panics —
    /// a truncated stream, a body that never carried usage, or a non-JSON
    /// tail all fall through to `None` rather than propagating a parse
    /// error, because "no usage found" is an ordinary, expected outcome
    /// here, not a bug.
    pub fn extract_usage(&mut self) -> Option<UsageTokens> {
        extract_usage(self.buf.make_contiguous())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

/// Two shapes to try, cheapest and most common first:
///
/// 1. The whole tail parses as one JSON object with a top-level `usage` —
///    true for a non-streaming completion whose entire body fit in the
///    buffer.
/// 2. Failing that, treat the tail as SSE and scan `data: {...}` lines for
///    one carrying `usage` — true for a streaming completion with
///    `stream_options.include_usage` set. The *last* matching line wins,
///    matching upstream behaviour of sending the usage chunk once, at the
///    end, right before `data: [DONE]`.
fn extract_usage(buf: &[u8]) -> Option<UsageTokens> {
    if let Some(u) = parse_usage_object(buf) {
        return Some(u);
    }
    let mut last = None;
    for line in buf.split(|&b| b == b'\n') {
        let line = trim_ascii(line);
        let Some(rest) = line.strip_prefix(b"data:") else {
            continue;
        };
        let rest = trim_ascii(rest);
        if rest == b"[DONE]" {
            continue;
        }
        if let Some(u) = parse_usage_object(rest) {
            last = Some(u);
        }
    }
    last
}

fn parse_usage_object(bytes: &[u8]) -> Option<UsageTokens> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let usage = value.get("usage")?;
    if usage.is_null() {
        return None;
    }
    let prompt_tokens = usage.get("prompt_tokens")?.as_u64()?;
    let completion_tokens = usage.get("completion_tokens")?.as_u64()?;
    Some(UsageTokens {
        prompt_tokens: prompt_tokens.min(u32::MAX as u64) as u32,
        completion_tokens: completion_tokens.min(u32::MAX as u64) as u32,
    })
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace());
    let Some(start) = start else { return &[] };
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).unwrap();
    &b[start..=end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_usage_chunk(prompt: u32, completion: u32) -> String {
        format!(
            "data: {{\"id\":\"c\",\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\
             \"completion_tokens\":{completion},\"total_tokens\":{}}}}}\n\ndata: [DONE]\n\n",
            prompt + completion
        )
    }

    #[test]
    fn finds_usage_in_a_streaming_sse_tail() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        tail.push(sse_usage_chunk(12, 34).as_bytes());
        let usage = tail.extract_usage().expect("usage chunk must be found");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 34);
    }

    #[test]
    fn finds_usage_in_a_non_streaming_body() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(
            br#"{"id":"c","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":5,"completion_tokens":6,"total_tokens":11}}"#,
        );
        let usage = tail.extract_usage().expect("usage must be found");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 6);
    }

    #[test]
    fn a_response_with_no_usage_at_all_extracts_nothing() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        tail.push(b"data: [DONE]\n\n");
        assert!(tail.extract_usage().is_none());

        let mut tail2 = TailBuffer::new(DEFAULT_CAPACITY);
        tail2.push(br#"{"id":"c","choices":[{"message":{"content":"hi"}}]}"#);
        assert!(tail2.extract_usage().is_none());
    }

    /// A stream that stops mid-frame (client hung up, upstream died) must
    /// never panic the extractor — the last, incomplete line simply fails to
    /// parse and is skipped.
    #[test]
    fn a_truncated_stream_does_not_panic_and_extracts_nothing() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"hi");
        assert!(tail.extract_usage().is_none());

        let mut empty = TailBuffer::new(DEFAULT_CAPACITY);
        assert!(empty.extract_usage().is_none());
    }

    /// A response far larger than the buffer must not panic, must not grow
    /// the buffer past `cap`, and — when the usage chunk lands within the
    /// last `cap` bytes, as it always does per the design ("usage always
    /// appears in the final event") — must still find it.
    #[test]
    fn a_body_larger_than_the_buffer_does_not_panic_and_still_finds_trailing_usage() {
        let cap = 256;
        let mut tail = TailBuffer::new(cap);
        for _ in 0..1000 {
            tail.push(b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\" x\"}}]}\n\n");
            assert!(tail.len() <= cap, "buffer must never exceed its capacity");
        }
        tail.push(sse_usage_chunk(1, 2).as_bytes());
        assert!(tail.len() <= cap);
        let usage = tail.extract_usage().expect("trailing usage must be found");
        assert_eq!(usage.prompt_tokens, 1);
        assert_eq!(usage.completion_tokens, 2);
    }

    /// One frame alone larger than the whole buffer must be handled without
    /// panicking (the `data.len() >= self.cap` branch in `push`), keeping
    /// only that frame's own tail.
    #[test]
    fn a_single_frame_larger_than_capacity_is_truncated_not_panicked_on() {
        let cap = 32;
        let mut tail = TailBuffer::new(cap);
        let huge = vec![b'x'; 1024];
        tail.push(&huge);
        assert_eq!(tail.len(), cap);
        assert!(tail.extract_usage().is_none());
    }

    #[test]
    fn a_null_usage_field_is_treated_as_absent() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(br#"{"id":"c","usage":null}"#);
        assert!(tail.extract_usage().is_none());
    }

    /// When several usage-bearing lines appear, the last one wins — matching
    /// upstream's actual behaviour of sending exactly one, at the end, but
    /// robust even if that assumption is ever violated.
    #[test]
    fn the_last_usage_chunk_in_the_tail_wins() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(sse_usage_chunk(1, 1).as_bytes());
        tail.push(sse_usage_chunk(99, 99).as_bytes());
        let usage = tail.extract_usage().unwrap();
        assert_eq!(usage.prompt_tokens, 99);
        assert_eq!(usage.completion_tokens, 99);
    }
}
