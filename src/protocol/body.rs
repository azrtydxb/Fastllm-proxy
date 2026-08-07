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
    pub reporter: UsageReporter,
}

impl UsageSink {
    fn record(self, usage: Usage) {
        // A response that produced nothing is not an event: reporting zeroes
        // would dilute nothing but would still cost a queue slot, and the
        // passthrough path reports nothing in the same situation.
        if usage.prompt_tokens == 0 && usage.completion_tokens == 0 {
            return;
        }
        self.reporter.record(UsageEvent {
            principal_id: self.principal_id,
            model: self.model,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            at: chrono::Utc::now(),
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
            if let Some(sink) = this.usage_sink.take() {
                if let Mode::Stream(t) = this.mode {
                    sink.record(t.usage());
                }
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
                    // Whatever was counted before the break is still owed.
                    if let Some(sink) = this.usage_sink.take() {
                        if let Mode::Stream(t) = this.mode {
                            sink.record(t.usage());
                        }
                    }
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    *this.done = true;
                    this.guard.take();
                    let out = match this.mode {
                        Mode::Stream(t) => {
                            let tail = t.finish();
                            if let Some(sink) = this.usage_sink.take() {
                                sink.record(t.usage());
                            }
                            tail
                        }
                        Mode::Buffer(buf) => {
                            match translate_response(*this.protocol, buf, this.ctx) {
                                Ok((bytes, usage)) => {
                                    if let Some(sink) = this.usage_sink.take() {
                                        sink.record(usage);
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
