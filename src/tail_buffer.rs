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
    /// What the provider says it charged, in micro-units, when it says.
    ///
    /// Authoritative where present, and strictly better than any price table:
    /// it is the amount actually billed, it already accounts for cache
    /// discounts and per-request routing, and it needs no maintenance when a
    /// provider changes its prices. OpenRouter returns it unasked; most
    /// providers return nothing, and those fall back to the configured price.
    pub cost_micros: Option<u64>,
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

/// Three shapes to try, cheapest and most common first:
///
/// 1. The whole tail parses as one JSON object with a top-level `usage` —
///    true for a non-streaming completion whose entire body fit in the
///    buffer.
/// 2. Failing that, treat the tail as SSE and scan `data: {...}` lines for
///    one carrying `usage` — true for a streaming completion with
///    `stream_options.include_usage` set. The *last* matching line wins,
///    matching upstream behaviour of sending the usage chunk once, at the
///    end, right before `data: [DONE]`.
/// 3. Failing that, read the object following the last `usage` key directly
///    — true for a non-streaming response *larger than the buffer*, where
///    the tail is a fragment of a JSON document rather than a document, so
///    (1) cannot parse it even though the counts are sitting in it.
fn extract_usage(buf: &[u8]) -> Option<UsageTokens> {
    if let Some(u) = parse_usage_object(buf) {
        return Some(u);
    }
    // One backwards search for the word, rather than a walk over every line.
    //
    // "The last matching line wins" is the same answer as "the first match
    // scanning from the end", and where that match sits is not a coincidence:
    // the usage chunk is the second-to-last line of the stream, immediately
    // before `data: [DONE]`. So a backwards search finds it within a couple of
    // hundred bytes, and a stream carrying no usage at all costs exactly one
    // contiguous scan of the tail instead of sixty small ones.
    //
    // Forwards and line by line, this parsed every delta frame in the 8 KiB
    // tail — around sixty full `serde_json::Value` trees, allocations and all
    // — to find something sitting at the end. Measured at 22-32 µs against a
    // request whose entire core cost is ~38 µs (`bench/micro`).
    let mut end = buf.len();
    while let Some(at) = rfind(&buf[..end], b"usage") {
        let line = line_around(buf, at);
        if let Some(rest) = line
            .strip_prefix(b"data:".as_slice())
            .map(trim_ascii)
            .filter(|r| *r != b"[DONE]")
        {
            if let Some(u) = parse_usage_object(rest) {
                return Some(u);
            }
        }
        // Not an SSE line. Before dismissing it, try reading the object that
        // follows this `usage` key directly.
        //
        // This is the case a tail window exists for and the whole-document
        // parse above cannot reach: a non-streaming response larger than the
        // buffer. The tail then holds the *end* of a JSON document — a
        // fragment with no opening brace — so parsing it as a document fails,
        // and before this the counts were dropped.
        //
        // It was not hypothetical. A single bge-m3 embeddings response is
        // around 22 KB against an 8 KiB window, so every embedding ever
        // served recorded `usage_reported = false` while the numbers sat
        // intact in the last hundred bytes of the very buffer being searched.
        // Any non-streaming completion longer than the window had the same
        // problem; the unit tests missed it because their fixtures fit.
        if let Some(obj) = balanced_object_at(buf, at) {
            if let Some(u) = serde_json::from_slice::<serde_json::Value>(obj)
                .ok()
                .as_ref()
                .and_then(usage_from_value)
            {
                return Some(u);
            }
        }
        // A genuine false positive — the word inside a message, say — so keep
        // searching before it.
        end = at;
    }
    None
}

/// The `\n`-delimited line containing `at`, trimmed.
fn line_around(buf: &[u8], at: usize) -> &[u8] {
    let start = buf[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let end = buf[at..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(buf.len(), |i| at + i);
    trim_ascii(&buf[start..end])
}

/// Offset of the last occurrence of `needle`.
///
/// Scans for one byte and only then compares, which is the shape the compiler
/// vectorises. The obvious `windows(n).rposition(..)` compares at every offset
/// instead, and measured about twice as slow over an 8 KiB tail.
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&last, head) = needle.split_last()?;
    let mut end = haystack.len();
    while end > head.len() {
        let at = haystack[head.len()..end].iter().rposition(|&b| b == last)? + head.len();
        if haystack[at - head.len()..at] == *head {
            return Some(at - head.len());
        }
        end = at;
    }
    None
}

fn parse_usage_object(bytes: &[u8]) -> Option<UsageTokens> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let usage = value.get("usage")?;
    usage_from_value(usage)
}

/// The `{ ... }` starting at or after `from`, as a slice.
///
/// Depth is tracked outside string literals only, so a brace inside a string
/// value cannot end the object early. Returns `None` if the object is not
/// closed within the buffer, which for a tail window means the object began
/// before the window did — there is nothing to parse and nothing to guess.
fn balanced_object_at(buf: &[u8], from: usize) -> Option<&[u8]> {
    let start = from + buf[from..].iter().position(|&b| b == b'{')?;
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    for (i, &b) in buf[start..].iter().enumerate() {
        if in_str {
            match () {
                _ if escaped => escaped = false,
                _ if b == b'\\' => escaped = true,
                _ if b == b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&buf[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read the token counts out of a `usage` object.
///
/// Both counts are optional, and at least one must be present. `usage: {}`
/// is not a report of zero consumption, but an embeddings response carrying
/// only `prompt_tokens` genuinely is a complete report — the OpenAI
/// embeddings API omits `completion_tokens` entirely, and requiring it meant
/// every embedding request was recorded as "counts unknown" no matter how
/// well the response was formed.
fn usage_from_value(usage: &serde_json::Value) -> Option<UsageTokens> {
    if usage.is_null() {
        return None;
    }
    let prompt = usage
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64);
    let completion = usage
        .get("completion_tokens")
        .and_then(serde_json::Value::as_u64);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let prompt_tokens = prompt.unwrap_or(0);
    let completion_tokens = completion.unwrap_or(0);
    // Dollars as a float on the wire — 4.8e-06 for a small request — so this
    // is the one place a float is unavoidable. Converted to integer micro-units
    // immediately and rounded rather than truncated: at these magnitudes a
    // request often costs single-digit micros, and truncating every one of them
    // would undercount systematically rather than symmetrically.
    let cost_micros = usage
        .get("cost")
        .and_then(serde_json::Value::as_f64)
        .filter(|c| c.is_finite() && *c >= 0.0)
        .map(|c| (c * 1_000_000.0).round() as u64);
    Some(UsageTokens {
        prompt_tokens: prompt_tokens.min(u32::MAX as u64) as u32,
        completion_tokens: completion_tokens.min(u32::MAX as u64) as u32,
        cost_micros,
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

    /// The word appearing in a model's own output must not be mistaken for a
    /// usage object. The backwards search stops at the *first* thing it finds,
    /// so a false positive nearer the end has to be stepped over rather than
    /// ending the search.
    #[test]
    fn the_word_usage_in_content_does_not_shadow_the_real_one() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\n",
        );
        tail.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"your usage is high\"}}]}\n\n");
        tail.push(b"data: [DONE]\n\n");
        assert_eq!(
            tail.extract_usage(),
            Some(UsageTokens {
                prompt_tokens: 7,
                completion_tokens: 9,
                cost_micros: None
            })
        );
    }

    /// Two usage chunks in the window: the last one is the real total, and
    /// billing the earlier one would undercount every request that had them.
    #[test]
    fn the_last_usage_chunk_wins() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n");
        tail.push(b"data: {\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":60}}\n\n");
        tail.push(b"data: [DONE]\n\n");
        assert_eq!(
            tail.extract_usage(),
            Some(UsageTokens {
                prompt_tokens: 50,
                completion_tokens: 60,
                cost_micros: None
            })
        );
    }

    #[test]
    fn rfind_finds_the_last_occurrence_and_nothing_that_is_not_there() {
        assert_eq!(rfind(b"usage x usage y", b"usage"), Some(8));
        assert_eq!(rfind(b"usage", b"usage"), Some(0));
        assert_eq!(rfind(b"usag", b"usage"), None);
        assert_eq!(rfind(b"", b"usage"), None);
        // A partial match at the very end must not be read as a hit.
        assert_eq!(rfind(b"xxusage_", b"usage"), Some(2));
    }

    #[test]
    fn line_around_returns_the_whole_line_from_anywhere_inside_it() {
        let buf = b"first\ndata: {\"usage\":1}\nlast";
        let at = buf.windows(5).position(|w| w == b"usage").unwrap();
        assert_eq!(line_around(buf, at), b"data: {\"usage\":1}");
        assert_eq!(line_around(buf, 0), b"first");
    }

    /// The provider's own figure, where it gives one. OpenRouter returns it
    /// unasked, in dollars as a float, and it is authoritative: it is what was
    /// billed, cache discounts and routed aliases included.
    #[test]
    fn a_provider_reported_cost_is_read_from_the_usage_object() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(br#"data: {"usage":{"prompt_tokens":8,"completion_tokens":6,"cost":4.8e-06}}"#);
        tail.push(b"\n\ndata: [DONE]\n\n");
        let got = tail.extract_usage().unwrap();
        assert_eq!(got.prompt_tokens, 8);
        // 4.8e-06 dollars is 4.8 micro-units. Rounded, not truncated: a small
        // request often costs single digits, and truncating every one of them
        // undercounts systematically rather than symmetrically.
        assert_eq!(got.cost_micros, Some(5));
    }

    #[test]
    fn a_provider_that_reports_no_cost_leaves_it_absent() {
        // Absent, not zero — the control plane prices it from the model's
        // configured rate instead, and zero would look like a free request.
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(br#"data: {"usage":{"prompt_tokens":8,"completion_tokens":6}}"#);
        tail.push(b"\n\ndata: [DONE]\n\n");
        assert_eq!(tail.extract_usage().unwrap().cost_micros, None);
    }

    #[test]
    fn a_nonsense_cost_is_ignored_rather_than_trusted() {
        // Not `1e400`: a number outside f64's range fails serde_json at the
        // parse, so the whole line yields nothing — the same all-or-nothing
        // behaviour any malformed JSON always had here, and not something
        // reading `cost` introduced.
        for bad in ["null", "-1", "\"free\"", "true"] {
            let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
            tail.push(
                format!(
                    r#"data: {{"usage":{{"prompt_tokens":1,"completion_tokens":1,"cost":{bad}}}}}"#
                )
                .as_bytes(),
            );
            tail.push(b"\n\ndata: [DONE]\n\n");
            assert_eq!(
                tail.extract_usage().unwrap().cost_micros,
                None,
                "cost {bad} must not be believed"
            );
        }
    }

    /// The bug this file's third strategy exists for: a non-streaming
    /// response *larger than the tail window*.
    ///
    /// The tail then holds the end of a JSON document rather than a
    /// document, so parsing it whole fails — while the counts sit intact in
    /// its last hundred bytes. Sized after the real case that exposed it: a
    /// bge-m3 embeddings response is roughly 22 KB against an 8 KiB window.
    #[test]
    fn a_non_streaming_body_larger_than_the_window_still_yields_its_usage() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        // A plausible embeddings body: a long float array, then usage last.
        let vector: String = (0..4000).map(|i| format!("{}.{:04},", i % 10, i)).collect();
        let body = format!(
            r#"{{"object":"list","data":[{{"embedding":[{vector}0.0]}}],"model":"bge-m3",
                 "usage":{{"prompt_tokens":4,"total_tokens":4,"completion_tokens":0}}}}"#
        );
        assert!(
            body.len() > DEFAULT_CAPACITY * 2,
            "fixture must exceed the window, or it proves nothing: {} bytes",
            body.len()
        );
        tail.push(body.as_bytes());

        let usage = tail
            .extract_usage()
            .expect("usage sits in the tail and must be found");
        assert_eq!(usage.prompt_tokens, 4);
        assert_eq!(usage.completion_tokens, 0);
    }

    /// An embeddings response that omits `completion_tokens` entirely, as
    /// the OpenAI embeddings API does. Requiring the field recorded every
    /// one of these as "counts unknown" despite a perfectly well-formed
    /// report.
    #[test]
    fn usage_without_completion_tokens_is_a_complete_report_not_a_missing_one() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(br#"{"object":"list","usage":{"prompt_tokens":7,"total_tokens":7}}"#);
        let usage = tail
            .extract_usage()
            .expect("prompt_tokens alone is a report");
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 0);
    }

    /// But an empty usage object is not a report of zero.
    #[test]
    fn an_empty_usage_object_is_not_a_report() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        tail.push(br#"{"object":"list","usage":{}}"#);
        assert_eq!(tail.extract_usage(), None);
    }

    /// A brace inside a string value must not close the object early. The
    /// scanner tracks depth outside string literals only, and getting that
    /// wrong truncates the object into something that will not parse — which
    /// would look exactly like "no usage found" rather than like a bug.
    #[test]
    fn a_brace_inside_a_string_does_not_end_the_usage_object() {
        let mut tail = TailBuffer::new(DEFAULT_CAPACITY);
        // Opens mid-document, so the tail cannot parse as one and strategy 3
        // is the one under test rather than strategy 1.
        tail.push(br#"ent":[0.1]}],"usage":{"note":"a } and a \" quote","prompt_tokens":3,"completion_tokens":9}}"#);
        let usage = tail
            .extract_usage()
            .expect("must parse past the brace in the string");
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 9);
    }
}
