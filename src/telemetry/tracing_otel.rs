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
    /// OTLP endpoint. `http://collector:4317` for gRPC,
    /// `https://api.host/otel/v1/traces` for HTTP — see [`Protocol::infer`].
    pub endpoint: String,
    /// Sample one request in this many. 1 traces everything.
    pub sample_one_in: u64,
    /// `service.name` on every span.
    pub service_name: String,
    /// Headers on every export — an `Authorization` for a hosted backend, and
    /// whatever routing header it needs beside it.
    ///
    /// Never logged. This is where an API key lives, and a startup line
    /// echoing the configuration would put it in the log collector, which is
    /// the one place it must not be. `Config` derives `Debug` for the rest of
    /// its fields; this one is printed as a count.
    pub headers: Vec<(String, String)>,
    /// `None` infers from the endpoint.
    pub protocol: Option<Protocol>,
}

/// Which OTLP transport to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Grpc,
    Http,
}

impl Protocol {
    /// Infer the transport from the endpoint.
    ///
    /// Every hosted backend publishes an HTTPS URL ending in `/v1/traces`, and
    /// the two ports are fixed by the specification: 4317 gRPC, 4318 HTTP. So
    /// the endpoint already says which it is, and asking the operator to say
    /// it again is a flag to get wrong. `--otel-protocol` overrides when the
    /// address is behind a proxy that hides both signals.
    pub fn infer(endpoint: &str) -> Self {
        if endpoint.contains("/v1/traces") || endpoint.contains(":4318") {
            Self::Http
        } else {
            Self::Grpc
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "grpc" => Some(Self::Grpc),
            "http" | "http-proto" | "http/protobuf" => Some(Self::Http),
            _ => None,
        }
    }
}

/// `key=value` pairs, as the flag and the env var both carry them.
///
/// Values may contain `=` — a base64 token routinely ends in one — so only the
/// first is a separator. A pair with no `=`, or an empty key, is dropped with a
/// warning rather than silently becoming a header named after the whole
/// string: a malformed auth header fails the export on every span, and the
/// operator needs to know which entry did it. The value is never logged.
pub fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|pair| match pair.split_once('=') {
            // The value is trimmed too. `k = v` is how a person writes a
            // list, and a leading space inside a bearer token fails
            // authentication on every span with nothing to show why. HTTP
            // strips surrounding whitespace from header values regardless.
            Some((k, v)) if !k.trim().is_empty() => {
                Some((k.trim().to_string(), v.trim().to_string()))
            }
            _ => {
                tracing::warn!(
                    "ignoring an OTLP header with no key=value form; \
                     the value is not shown because it may be a credential"
                );
                None
            }
        })
        .collect()
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

    let protocol = cfg
        .protocol
        .unwrap_or_else(|| Protocol::infer(&cfg.endpoint));
    // Counted, not listed: a header value here is routinely an API key.
    tracing::info!(
        endpoint = %cfg.endpoint,
        protocol = ?protocol,
        headers = cfg.headers.len(),
        "exporting spans"
    );
    let exporter = match protocol {
        Protocol::Grpc => {
            use opentelemetry_otlp::WithTonicConfig as _;
            let mut md = tonic::metadata::MetadataMap::new();
            for (k, v) in &cfg.headers {
                match (
                    k.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>(),
                    v.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>(),
                ) {
                    (Ok(key), Ok(val)) => {
                        md.insert(key, val);
                    }
                    // Named, never shown: a rejected header is usually the
                    // credential, and the operator needs to know which entry
                    // failed without it reaching the log collector.
                    _ => tracing::warn!(
                        header = %k,
                        "dropping an OTLP header gRPC will not accept; value not shown"
                    ),
                }
            }
            opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(&cfg.endpoint)
                .with_metadata(md)
                .build()?
        }
        Protocol::Http => {
            use opentelemetry_otlp::WithHttpConfig as _;
            opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(&cfg.endpoint)
                .with_headers(cfg.headers.iter().cloned().collect())
                .build()?
        }
    };

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

#[cfg(test)]
mod otlp_transport_tests {
    use super::*;

    /// The two ports are fixed by the specification and every hosted backend
    /// publishes a `/v1/traces` URL, so the endpoint already says which
    /// transport it is. Getting this wrong exports to a port that is not
    /// listening and drops every span with no error the operator sees.
    #[test]
    fn the_transport_is_read_from_the_endpoint() {
        for grpc in [
            "http://collector:4317",
            "https://otel.internal:4317",
            "http://localhost",
        ] {
            assert_eq!(Protocol::infer(grpc), Protocol::Grpc, "{grpc}");
        }
        for http in [
            "https://api.braintrust.dev/otel/v1/traces",
            "https://api.honeycomb.io/v1/traces",
            "http://collector:4318",
        ] {
            assert_eq!(Protocol::infer(http), Protocol::Http, "{http}");
        }
    }

    /// A base64 credential routinely ends in `=`, so only the first one
    /// separates. Splitting on every `=` truncates the token and the export
    /// fails authentication on every span.
    #[test]
    fn a_header_value_may_contain_equals_signs() {
        let got = parse_headers("Authorization=Bearer abc==,x-bt-parent=project_id:123");
        assert_eq!(
            got,
            vec![
                ("Authorization".to_string(), "Bearer abc==".to_string()),
                ("x-bt-parent".to_string(), "project_id:123".to_string()),
            ]
        );
    }

    /// Whitespace around a comma-separated list is what a shell or a YAML
    /// block scalar leaves behind.
    #[test]
    fn surrounding_whitespace_is_not_part_of_the_key() {
        let got = parse_headers("  Authorization = Bearer t  ,  k=v ");
        assert_eq!(got[0].0, "Authorization");
        assert_eq!(
            got[0].1, "Bearer t",
            "no leading space survives into the credential"
        );
        assert_eq!(got[1], ("k".to_string(), "v".to_string()));
    }

    /// A malformed entry is dropped rather than becoming a header named after
    /// the whole string — which would fail the export on every span with a
    /// message naming a header nobody configured.
    #[test]
    fn an_entry_with_no_equals_is_dropped_not_guessed_at() {
        assert!(parse_headers("justatoken").is_empty());
        assert!(parse_headers("=novalue").is_empty());
        assert!(parse_headers("").is_empty());
        assert_eq!(parse_headers("a=1,broken,b=2").len(), 2);
    }

    /// An explicit flag has to win, or an endpoint behind a proxy that hides
    /// both signals could never be corrected.
    #[test]
    fn an_explicit_protocol_overrides_the_endpoint() {
        assert_eq!(Protocol::parse("http"), Some(Protocol::Http));
        assert_eq!(Protocol::parse("GRPC"), Some(Protocol::Grpc));
        assert_eq!(Protocol::parse("http/protobuf"), Some(Protocol::Http));
        assert_eq!(Protocol::parse("carrier-pigeon"), None);
    }
}
