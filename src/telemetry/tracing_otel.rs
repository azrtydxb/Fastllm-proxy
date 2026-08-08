//! OTLP tracing, behind the `otel` feature.
//!
//! # What this costs, and where
//!
//! A span is the most expensive instrument in this crate: creating one
//! allocates, records attributes, and hands it to an exporter. That is fine —
//! spans answer questions metrics cannot, like *which* request was slow and
//! what it did — but it is not something to pay on every request at the volumes
//! this proxy runs at.
//!
//! So two gates, in this order:
//!
//! 1. **The feature.** A build without `otel` has no exporter, no dependency
//!    tree, and no branch: the instrumentation compiles away.
//! 2. **A sampler.** With the feature on, a head sampler decides once per trace
//!    whether to record it. An unsampled request pays a comparison against an
//!    atomic counter and nothing else — no allocation, no attributes.
//!
//! Export is a background batch task. Nothing on the request path ever writes
//! to a socket, which is the same rule the usage reporter follows and the
//! reason neither can make inference wait on a collector being reachable.
//!
//! # Why deterministic sampling rather than random
//!
//! `TraceIdRatioBased` is the obvious choice, but it needs a random source per
//! request, and this crate deliberately has no RNG in a `--no-default-features`
//! build. Counting is cheaper, needs no RNG, and gives an exact ratio rather
//! than one that is only right on average — at the cost of being predictable,
//! which matters for an adversary trying to avoid being traced and not at all
//! for finding a slow request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use opentelemetry::trace::{SamplingDecision, SamplingResult, TraceState};
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::trace::ShouldSample;

/// Samples one trace in `n`, by counting.
///
/// The counter is shared rather than copied: the SDK clones the sampler, and a
/// per-clone counter would sample one in `n` *per clone*, quietly multiplying
/// the rate by however many the SDK happened to make.
#[derive(Debug, Clone)]
struct EveryNth {
    n: u64,
    seen: Arc<AtomicU64>,
}

impl EveryNth {
    fn new(n: u64) -> Self {
        Self {
            n: n.max(1),
            seen: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ShouldSample for EveryNth {
    fn should_sample(
        &self,
        parent: Option<&opentelemetry::Context>,
        _trace_id: opentelemetry::trace::TraceId,
        _name: &str,
        _kind: &opentelemetry::trace::SpanKind,
        _attributes: &[KeyValue],
        _links: &[opentelemetry::trace::Link],
    ) -> SamplingResult {
        use opentelemetry::trace::TraceContextExt as _;
        // Honour an upstream decision before making one. A caller that already
        // sampled this trace is asking for the whole path, and a proxy that
        // drops its own span leaves a hole in the middle of somebody else's
        // trace — which is worse than not tracing at all, because it looks
        // like the request never reached us.
        if let Some(parent) = parent {
            let span = parent.span();
            let ctx = span.span_context();
            if ctx.is_valid() {
                return SamplingResult {
                    decision: if ctx.is_sampled() {
                        SamplingDecision::RecordAndSample
                    } else {
                        SamplingDecision::Drop
                    },
                    attributes: Vec::new(),
                    trace_state: ctx.trace_state().clone(),
                };
            }
        }
        let n = self.seen.fetch_add(1, Ordering::Relaxed);
        SamplingResult {
            decision: if n % self.n == 0 {
                SamplingDecision::RecordAndSample
            } else {
                SamplingDecision::Drop
            },
            attributes: Vec::new(),
            trace_state: TraceState::default(),
        }
    }
}

/// How tracing is configured, from the CLI.
#[derive(Debug, Clone)]
pub struct Config {
    /// OTLP/gRPC endpoint, e.g. `http://collector:4317`.
    pub endpoint: String,
    /// Sample one request in this many. 1 traces everything.
    pub sample_one_in: u64,
    /// `service.name` on every span.
    pub service_name: String,
}

/// Build the OTLP layer, or explain why it could not be built.
///
/// Returns a `tracing` layer rather than installing a subscriber, so the caller
/// keeps one subscriber with the log layer alongside it — two subscribers would
/// mean the second `init` silently losing to the first.
pub fn layer<S>(cfg: &Config) -> anyhow::Result<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use opentelemetry::trace::TracerProvider as _;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&cfg.endpoint)
        .build()?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        // Batched on a background task: the request path never waits on the
        // collector, and a collector that is down costs dropped spans rather
        // than dropped requests.
        .with_batch_exporter(exporter)
        .with_sampler(EveryNth::new(cfg.sample_one_in))
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(cfg.service_name.clone())
                .build(),
        )
        .build();

    let tracer = provider.tracer("fastllm-proxy");
    // Held for the process lifetime so the batch task keeps running; dropping
    // the provider stops the exporter and silently ends tracing.
    opentelemetry::global::set_tracer_provider(provider);

    Ok(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Flush anything still batched.
///
/// Called on shutdown: without it the last few seconds of spans — usually the
/// ones explaining why the process is going down — are lost with the buffer.
pub fn shutdown() {
    // 0.31 has no global shutdown; the provider set above flushes on drop, and
    // the SDK's own shutdown hook runs at process exit. This exists as the
    // single place to change if that stops being true.
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanKind, TraceId};

    fn decide(s: &EveryNth) -> SamplingDecision {
        s.should_sample(None, TraceId::INVALID, "req", &SpanKind::Server, &[], &[])
            .decision
    }

    #[test]
    fn one_in_n_is_exact_rather_than_only_right_on_average() {
        let s = EveryNth::new(4);
        let sampled = (0..400)
            .filter(|_| decide(&s) == SamplingDecision::RecordAndSample)
            .count();
        assert_eq!(
            sampled, 100,
            "counting gives an exact ratio; a random sampler would only \
             approach it, and at low volume 'approach' means a quiet hour \
             traces nothing"
        );
    }

    #[test]
    fn a_ratio_of_one_traces_everything_and_zero_is_treated_as_one() {
        let s = EveryNth::new(1);
        assert!((0..10).all(|_| decide(&s) == SamplingDecision::RecordAndSample));
        // 0 would be a divide by zero; clamping beats panicking on a flag value
        // somebody typed.
        let s = EveryNth::new(0);
        assert_eq!(decide(&s), SamplingDecision::RecordAndSample);
    }
}
