//! The response half of translation: upstream frames in, OpenAI frames out.
//!
//! This is the only place in the proxy where a response body is parsed, and it
//! is reached only for a backend an operator explicitly configured for a native
//! protocol. `TrackedBody` — the passthrough body — still forwards bytes it
//! never reads, and `tests/passthrough_is_byte_exact.rs` pins that the two do
//! not blur together.
//!
//! Usage falls out of translation rather than being recovered afterwards. The
//! passthrough path mirrors bytes into a bounded [`TailBuffer`] and parses the
//! tail once at end of stream, because it has no other way to see the numbers.
//! Here the numbers have already been parsed exactly, so a translated response
//! carries no tail buffer at all.
//!
//! [`TailBuffer`]: crate::tail_buffer::TailBuffer

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{translate_response, Protocol, ResponseContext, StreamTranslator, Usage};
use crate::registry::InflightGuard;
use crate::snapshot::PrincipalId;
use crate::upstream::UpstreamBody;
use crate::usage::{UsageEvent, UsageReporter};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Where a translated response's token counts go. The tail-buffer-free
/// counterpart of `proxy::UsageTracking`.
pub struct UsageSink {
    pub principal_id: PrincipalId,
    pub model: String,
    /// What the client asked for, when it differs from `model`.
    pub requested_model: Option<String>,
    pub status: u16,
    pub reporter: UsageReporter,
}

impl UsageSink {
    /// Record this request, with or without token counts.
    ///
    /// `usage` is `None` when the translator never saw a usage block — a
    /// buffered response that carried none, an upstream error, a stream that
    /// ended early. The row is still written: it is what makes request rate
    /// and error rate answerable from `usage_events`, and an error response
    /// is precisely the case that never carries usage, so dropping these
    /// would drop exactly the rows an error chart is made of.
    ///
    /// All-zero counts are treated as "not reported" rather than as a real
    /// measurement of nothing. That is what they mean here: the translators
    /// initialise their counters to zero and only move them when a usage
    /// block says so, so zero is the absence of a report, not a report of
    /// absence. The passthrough path in `proxy::UsageTracking` draws the
    /// same distinction, and the two must agree or the same request would be
    /// counted differently depending on which backend served it.
    fn record(self, usage: Option<Usage>, timing: Option<&crate::telemetry::RequestTiming>) {
        let reported = usage
            .as_ref()
            .is_some_and(|u| u.prompt_tokens != 0 || u.completion_tokens != 0);
        self.reporter.record(UsageEvent {
            principal_id: self.principal_id,
            model: self.model,
            prompt_tokens: usage.as_ref().map_or(0, |u| u.prompt_tokens),
            completion_tokens: usage.as_ref().map_or(0, |u| u.completion_tokens),
            usage_reported: reported,
            // A backend answered; this row is not a refusal.
            refusal: None,
            at: chrono::Utc::now(),
            duration_ms: timing.map(|t| t.duration_ms()),
            ttft_ms: timing.and_then(|t| t.ttft_ms()),
            status: Some(self.status),
            requested_model: self.requested_model,
            // A translated backend's usage comes from the translator, which
            // reports tokens and not money; the control plane prices it.
            cost_micros: None,
        });
    }
}

/// Streaming translates incrementally; non-streaming has to see the whole
/// document before it can say anything, so it accumulates.
enum Mode {
    Stream(StreamTranslator),
    Buffer(Vec<u8>),
}

pin_project! {
    pub struct TranslatedBody {
        #[pin]
        inner: UpstreamBody,
        guard: Option<InflightGuard>,
        mode: Mode,
        protocol: Protocol,
        ctx: ResponseContext,
        usage_sink: Option<UsageSink>,
        // Taken once, so a body finished by `poll_frame` is not recorded again
        // by `PinnedDrop` — the same rule `usage_sink` above follows.
        timing: Option<crate::telemetry::RequestTiming>,
        // Set once the upstream body has ended and the trailing frame (final
        // chunk plus `[DONE]`, or the translated document) has been handed
        // out. Also what `is_end_stream` reports.
        done: bool,
    }

    impl PinnedDrop for TranslatedBody {
        /// Same trap as `proxy::TrackedBody`'s: a client that hangs up
        /// mid-generation drops this body without ever polling it to the end,
        /// and those tokens were still consumed upstream. Reporting whatever
        /// the translator has counted so far is the right way to be wrong.
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let streamed_usage = match &this.mode {
                Mode::Stream(t) => Some(t.usage()),
                Mode::Buffer(_) => None,
            };
            // Unconditionally, even in `Buffer` mode where `streamed_usage`
            // is `None`: a sink still held at drop is a request that ended
            // without any usage ever being seen, and that is a row worth
            // having rather than a row to skip. See `UsageSink::record`.
            if let Some(sink) = this.usage_sink.take() {
                sink.record(streamed_usage, this.timing.as_ref());
            }
            if let Some(timing) = this.timing.take() {
                // Tokens come from the translator's own count rather than the
                // usage sink, so they are recorded whether or not this
                // principal's consumption is reported to the control plane.
                if let Some(usage) = streamed_usage {
                    timing.record_tokens(usage.prompt_tokens, usage.completion_tokens);
                }
                timing.finish();
            }
        }
    }
}

impl TranslatedBody {
    pub fn new(
        inner: UpstreamBody,
        guard: InflightGuard,
        protocol: Protocol,
        ctx: ResponseContext,
        usage_sink: Option<UsageSink>,
        timing: Option<crate::telemetry::RequestTiming>,
    ) -> Self {
        let mode = match ctx.streaming {
            true => match StreamTranslator::new(protocol, ctx.clone()) {
                Some(t) => Mode::Stream(t),
                // Unreachable for a translated backend, and a buffer is the
                // safe reading of it if it ever happens.
                None => Mode::Buffer(Vec::new()),
            },
            false => Mode::Buffer(Vec::new()),
        };
        Self {
            inner,
            guard: Some(guard),
            mode,
            protocol,
            ctx,
            usage_sink,
            timing,
            done: false,
        }
    }

    pub fn boxed_body(self) -> http_body_util::combinators::BoxBody<Bytes, BoxError> {
        self.boxed()
    }
}

impl Body for TranslatedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        // A loop rather than one poll per call: an upstream frame can translate
        // to nothing at all (Anthropic's `ping`, a `content_block_start`), and
        // returning an empty data frame for it would be a protocol error toward
        // the client. Those frames are consumed and the upstream polled again.
        loop {
            if *this.done {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let Some(data) = frame.data_ref() else {
                        // Trailers carry nothing a chat completion needs, and
                        // forwarding them past a body we rewrote would be
                        // describing bytes that no longer exist.
                        continue;
                    };
                    match this.mode {
                        Mode::Stream(t) => {
                            let out = t.push(data);
                            if out.is_empty() {
                                continue;
                            }
                            // Measured at the first frame *we* emit, not the
                            // first the upstream sent: several upstream frames
                            // (`message_start`, `ping`, `content_block_start`)
                            // translate to nothing, and timing those would
                            // report a first token that the client never saw.
                            if let Some(timing) = this.timing.as_mut() {
                                timing.first_byte();
                            }
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                        }
                        Mode::Buffer(buf) => {
                            buf.extend_from_slice(data);
                            continue;
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    *this.done = true;
                    this.guard.take();
                    // Whatever was counted before the break is still owed —
                    // and a buffered response that broke owes a row too, with
                    // no counts on it, so the failure is visible.
                    if let Some(sink) = this.usage_sink.take() {
                        let counted = match this.mode {
                            Mode::Stream(t) => Some(t.usage()),
                            Mode::Buffer(_) => None,
                        };
                        sink.record(counted, this.timing.as_ref());
                    }
                    if let Some(timing) = this.timing.take() {
                        if let Mode::Stream(t) = this.mode {
                            let usage = t.usage();
                            timing.record_tokens(usage.prompt_tokens, usage.completion_tokens);
                        }
                        timing.finish();
                    }
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    *this.done = true;
                    this.guard.take();
                    let out = match this.mode {
                        Mode::Stream(t) => {
                            let tail = t.finish();
                            let usage = t.usage();
                            if let Some(sink) = this.usage_sink.take() {
                                sink.record(Some(usage), this.timing.as_ref());
                            }
                            if let Some(timing) = this.timing.take() {
                                timing.record_tokens(usage.prompt_tokens, usage.completion_tokens);
                                timing.finish();
                            }
                            tail
                        }
                        Mode::Buffer(buf) => {
                            match translate_response(*this.protocol, buf, this.ctx) {
                                Ok((bytes, usage)) => {
                                    if let Some(sink) = this.usage_sink.take() {
                                        sink.record(Some(usage), this.timing.as_ref());
                                    }
                                    if let Some(timing) = this.timing.take() {
                                        timing.record_tokens(
                                            usage.prompt_tokens,
                                            usage.completion_tokens,
                                        );
                                        // A buffered response has no separate
                                        // first-byte moment: the whole document
                                        // goes out at once, which is what
                                        // `first_byte` declines to record for a
                                        // non-streaming request.
                                        timing.finish();
                                    }
                                    bytes
                                }
                                // The upstream said something this build cannot
                                // read. The response is already committed —
                                // status and headers went out long ago — so the
                                // only honest signal left is to fail the body
                                // rather than hand the client a plausible-looking
                                // empty completion.
                                Err(e) => {
                                    tracing::warn!(
                                        protocol = %this.protocol.as_str(),
                                        error = %e,
                                        "could not translate upstream response"
                                    );
                                    return Poll::Ready(Some(Err(Box::new(e) as BoxError)));
                                }
                            }
                        }
                    };
                    if out.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// Reports this body's state, never the upstream's.
    ///
    /// Deferring to `inner` would be the subtle bug: a non-streaming upstream
    /// response carries a `content-length`, so the inner body declares itself
    /// ended as soon as its last frame is out — at which point hyper stops
    /// polling, and the translated document, which is only produced *after*
    /// that point, would never be written. The client would receive an empty
    /// 200.
    fn is_end_stream(&self) -> bool {
        self.done
    }
}
