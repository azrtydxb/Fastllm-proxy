//! The counters and histograms the request path writes.
//!
//! # Where these live, and why not on the registry
//!
//! Per-backend counters live on `Backend`, which survives a snapshot rebuild
//! because the registry carries live objects forward by uid. Pools do not: they
//! are rebuilt from scratch on every apply, about once a second. Per-model
//! metrics hung off a pool would therefore reset every second, and a counter
//! that resets is worse than no counter — Prometheus reads a reset as a process
//! restart and `rate()` silently invents a spike.
//!
//! So per-model metrics live here, on `AppState`, in a map rebuilt by
//! `apply_snapshot` alongside the registry and the classifier — the same rule
//! those follow, for the same reason: views derived from one snapshot must not
//! be able to diverge. Entries are carried forward by model name, so counters
//! survive every rebuild and only a model that genuinely went away loses them.
//!
//! # Cardinality
//!
//! Prometheus labels are restricted to values fixed by configuration: model
//! name, backend, and a small closed set of outcomes. Nothing here is labelled
//! by caller, key, or anything else a client controls — that is how a metrics
//! endpoint turns into an outage. Per-caller and per-request detail goes to the
//! usage channel instead, which is batched off the request path and lands in
//! Postgres, where high cardinality is ordinary.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;

use super::Histogram;

/// How a request ended, as a bounded set of label values.
///
/// Closed on purpose. Labelling by HTTP status would let an upstream inventing
/// a new code add a time series, and labelling by error message would let a
/// caller do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Answered by an upstream with a 2xx.
    Ok,
    /// Refused here, before any upstream was contacted: unknown model, not
    /// authorised, over a rate limit or budget, body too large.
    Rejected,
    /// An upstream answered with an error status.
    UpstreamError,
    /// No upstream could be reached, or every candidate was exhausted.
    Unavailable,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected => "rejected",
            Self::UpstreamError => "upstream_error",
            Self::Unavailable => "unavailable",
        }
    }

    pub const ALL: [Outcome; 4] = [
        Self::Ok,
        Self::Rejected,
        Self::UpstreamError,
        Self::Unavailable,
    ];
}

/// Why a request was refused before reaching an upstream.
///
/// Separate from [`Outcome`] because "rejected" is the answer to *what
/// happened* and this is the answer to *why*, and an operator watching a spike
/// needs the second one to know whether to look at keys, limits or budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Unauthenticated,
    Unauthorised,
    ModelNotFound,
    RateLimited,
    OverBudget,
    BodyTooLarge,
    Unsupported,
}

impl Rejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::Unauthorised => "unauthorised",
            Self::ModelNotFound => "model_not_found",
            Self::RateLimited => "rate_limited",
            Self::OverBudget => "over_budget",
            Self::BodyTooLarge => "body_too_large",
            Self::Unsupported => "unsupported",
        }
    }

    pub const ALL: [Rejection; 7] = [
        Self::Unauthenticated,
        Self::Unauthorised,
        Self::ModelNotFound,
        Self::RateLimited,
        Self::OverBudget,
        Self::BodyTooLarge,
        Self::Unsupported,
    ];
}

/// Metrics for one model, carried across snapshot rebuilds by name.
#[derive(Debug, Default)]
pub struct ModelMetrics {
    pub requests: AtomicU64,
    /// Whole-request wall time, from the proxy accepting it to the last byte.
    pub duration: Histogram,
    /// Time to first byte of the response body. Streaming only — for a
    /// non-streaming answer it is the same number as `duration` and recording
    /// it twice would only make the two look independent.
    pub ttft: Histogram,
    pub prompt_tokens: AtomicU64,
    pub completion_tokens: AtomicU64,
}

/// Everything the request path records.
#[derive(Debug, Default)]
pub struct Telemetry {
    pub duration: Histogram,
    pub ttft: Histogram,
    /// Requests by outcome, indexed by [`Outcome`]'s discriminant order.
    outcomes: [AtomicU64; Outcome::ALL.len()],
    rejections: [AtomicU64; Rejection::ALL.len()],
    /// Retries within one model's pool, and moves to a different model.
    /// Separate because they mean different things: the first is a replica
    /// having a bad moment, the second is a model being unusable.
    pub retries: AtomicU64,
    pub failovers: AtomicU64,
    /// How often the deployment-wide fallback actually caught something. If
    /// this is never zero, a routing rule is wrong somewhere upstream of it.
    pub fallback_used: AtomicU64,
    /// Prompts classified by each tier, and prompts that cleared no floor.
    /// The ratio of the second to the first is what says whether tier 2 is
    /// earning its 3.3ms.
    pub classified_fast: AtomicU64,
    pub classified_refined: AtomicU64,
    pub unclassified: AtomicU64,
    models: ArcSwap<HashMap<String, Arc<ModelMetrics>>>,
}

impl Telemetry {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn record_outcome(&self, outcome: Outcome) {
        self.outcomes[outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_rejection(&self, why: Rejection) {
        self.rejections[why as usize].fetch_add(1, Ordering::Relaxed);
        self.record_outcome(Outcome::Rejected);
    }

    /// Metrics for `model`, or `None` if it is not in the current snapshot.
    ///
    /// Called once per request, not once per observation: the returned handle
    /// is carried alongside the response body so the whole-request numbers can
    /// be recorded when it finishes without a second lookup.
    pub fn model(&self, model: &str) -> Option<Arc<ModelMetrics>> {
        self.models.load().get(model).cloned()
    }

    /// Refresh the per-model map from a snapshot's model list.
    ///
    /// Existing entries are carried forward by name so their counters survive;
    /// only a model that is genuinely gone loses its history. Called from
    /// `AppState::apply_snapshot`, alongside the registry and classifier
    /// rebuilds, because a derived view refreshed anywhere else is one that can
    /// drift from the snapshot it claims to describe.
    pub fn rebuild_models<'a>(&self, names: impl Iterator<Item = &'a str>) {
        let existing = self.models.load();
        let next: HashMap<String, Arc<ModelMetrics>> = names
            .map(|name| {
                let metrics = existing
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(ModelMetrics::default()));
                (name.to_string(), metrics)
            })
            .collect();
        self.models.store(Arc::new(next));
    }

    /// Render everything in the Prometheus text format.
    ///
    /// The only place in this module that allocates or formats, and it runs on
    /// a scrape rather than on a request — which is what lets every write above
    /// be a single atomic add.
    pub fn render(&self, out: &mut String) {
        out.push_str("# HELP fastllm_request_outcomes_total Requests by how they ended.\n");
        out.push_str("# TYPE fastllm_request_outcomes_total counter\n");
        for outcome in Outcome::ALL {
            out.push_str(&format!(
                "fastllm_request_outcomes_total{{outcome=\"{}\"}} {}\n",
                outcome.as_str(),
                self.outcomes[outcome as usize].load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP fastllm_rejections_total Requests refused here, before any upstream was \
             contacted, by reason.\n",
        );
        out.push_str("# TYPE fastllm_rejections_total counter\n");
        for why in Rejection::ALL {
            out.push_str(&format!(
                "fastllm_rejections_total{{reason=\"{}\"}} {}\n",
                why.as_str(),
                self.rejections[why as usize].load(Ordering::Relaxed)
            ));
        }

        for (name, help, value) in [
            (
                "fastllm_retries_total",
                "Retries onto another backend of the same model.",
                &self.retries,
            ),
            (
                "fastllm_failovers_total",
                "Moves to a different model after one was unusable.",
                &self.failovers,
            ),
            (
                "fastllm_fallback_used_total",
                "Requests the deployment-wide fallback model caught.",
                &self.fallback_used,
            ),
            (
                "fastllm_classified_fast_total",
                "Prompts classified by the static embedding tier.",
                &self.classified_fast,
            ),
            (
                "fastllm_classified_refined_total",
                "Prompts the transformer tier decided, a subset of those escalated.",
                &self.classified_refined,
            ),
            (
                "fastllm_unclassified_total",
                "Prompts that cleared no class's confidence floor.",
                &self.unclassified,
            ),
        ] {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
            out.push_str(&format!("{name} {}\n", value.load(Ordering::Relaxed)));
        }

        out.push_str("# HELP fastllm_request_duration_seconds Whole-request wall time.\n");
        out.push_str("# TYPE fastllm_request_duration_seconds histogram\n");
        self.duration
            .render(out, "fastllm_request_duration_seconds", "");

        out.push_str(
            "# HELP fastllm_time_to_first_token_seconds Time to the first byte of a streamed \
             response body.\n",
        );
        out.push_str("# TYPE fastllm_time_to_first_token_seconds histogram\n");
        self.ttft
            .render(out, "fastllm_time_to_first_token_seconds", "");

        let models = self.models.load();
        // Sorted so a scrape is byte-stable between calls with no traffic,
        // which makes diffing two scrapes a usable debugging tool.
        let mut names: Vec<&String> = models.keys().collect();
        names.sort();

        out.push_str("# HELP fastllm_model_requests_total Requests routed to a model.\n");
        out.push_str("# TYPE fastllm_model_requests_total counter\n");
        for name in &names {
            let m = &models[*name];
            out.push_str(&format!(
                "fastllm_model_requests_total{{model=\"{name}\"}} {}\n",
                m.requests.load(Ordering::Relaxed)
            ));
        }

        out.push_str("# HELP fastllm_model_tokens_total Tokens reported by the upstream.\n");
        out.push_str("# TYPE fastllm_model_tokens_total counter\n");
        for name in &names {
            let m = &models[*name];
            out.push_str(&format!(
                "fastllm_model_tokens_total{{model=\"{name}\",kind=\"prompt\"}} {}\n",
                m.prompt_tokens.load(Ordering::Relaxed)
            ));
            out.push_str(&format!(
                "fastllm_model_tokens_total{{model=\"{name}\",kind=\"completion\"}} {}\n",
                m.completion_tokens.load(Ordering::Relaxed)
            ));
        }

        out.push_str("# HELP fastllm_model_duration_seconds Whole-request wall time, per model.\n");
        out.push_str("# TYPE fastllm_model_duration_seconds histogram\n");
        for name in &names {
            models[*name].duration.render(
                out,
                "fastllm_model_duration_seconds",
                &format!("model=\"{name}\""),
            );
        }

        out.push_str(
            "# HELP fastllm_model_time_to_first_token_seconds Time to first streamed byte, per \
             model.\n",
        );
        out.push_str("# TYPE fastllm_model_time_to_first_token_seconds histogram\n");
        for name in &names {
            models[*name].ttft.render(
                out,
                "fastllm_model_time_to_first_token_seconds",
                &format!("model=\"{name}\""),
            );
        }
    }
}

/// Whole-request timing, recorded when the response body finishes.
///
/// Lives on the body rather than wrapping the handler because a streamed
/// response is not over when the handler returns — it is over when the last
/// frame goes out, which can be a minute later. Timing the handler would report
/// the time to *start* answering as if it were the time to answer.
///
/// Two clock reads per request (`Instant::now`, 14ns each in `bench/micro`) and
/// one predictable branch per frame until the first byte is out.
pub struct RequestTiming {
    start: std::time::Instant,
    telemetry: Arc<Telemetry>,
    model: Option<Arc<ModelMetrics>>,
    awaiting_first_byte: bool,
    streaming: bool,
}

impl RequestTiming {
    pub fn new(
        telemetry: &Arc<Telemetry>,
        model: &str,
        streaming: bool,
        start: std::time::Instant,
    ) -> Self {
        Self {
            start,
            model: telemetry.model(model),
            telemetry: Arc::clone(telemetry),
            awaiting_first_byte: true,
            streaming,
        }
    }

    /// The first byte of the response body reached the client.
    #[inline]
    pub fn first_byte(&mut self) {
        if !self.awaiting_first_byte {
            return;
        }
        self.awaiting_first_byte = false;
        // Streamed responses only. For a buffered one this is the same instant
        // as completion, and publishing it as a separate series would suggest
        // the two are independent measurements when one is a copy of the other.
        if !self.streaming {
            return;
        }
        let us = self.start.elapsed().as_micros() as u64;
        self.telemetry.ttft.record_us(us);
        if let Some(m) = &self.model {
            m.ttft.record_us(us);
        }
    }

    /// Tokens this response reported, for the per-model counters.
    pub fn record_tokens(&self, prompt: u32, completion: u32) {
        if let Some(m) = &self.model {
            m.prompt_tokens
                .fetch_add(u64::from(prompt), Ordering::Relaxed);
            m.completion_tokens
                .fetch_add(u64::from(completion), Ordering::Relaxed);
        }
    }

    pub fn finish(&self) {
        let us = self.start.elapsed().as_micros() as u64;
        self.telemetry.duration.record_us(us);
        if let Some(m) = &self.model {
            m.duration.record_us(us);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejection_counts_as_both_its_reason_and_an_outcome() {
        // Two questions an operator asks in sequence — "what changed?" then
        // "why?" — and answering only the second means the first has to be
        // summed across reasons to be asked at all.
        let t = Telemetry::new();
        t.record_rejection(Rejection::RateLimited);
        t.record_rejection(Rejection::OverBudget);

        let mut out = String::new();
        t.render(&mut out);
        assert!(out.contains("fastllm_rejections_total{reason=\"rate_limited\"} 1"));
        assert!(out.contains("fastllm_rejections_total{reason=\"over_budget\"} 1"));
        assert!(out.contains("fastllm_request_outcomes_total{outcome=\"rejected\"} 2"));
    }

    /// Every label value is published from the first scrape, at zero, rather
    /// than appearing when it first happens. A series that springs into
    /// existence reads as "no data" on a dashboard until it fires once, which
    /// is indistinguishable from the endpoint being broken.
    #[test]
    fn every_bounded_label_is_present_before_it_ever_fires() {
        let mut out = String::new();
        Telemetry::new().render(&mut out);
        for outcome in Outcome::ALL {
            assert!(
                out.contains(&format!(
                    "fastllm_request_outcomes_total{{outcome=\"{}\"}} 0",
                    outcome.as_str()
                )),
                "missing {}",
                outcome.as_str()
            );
        }
        for why in Rejection::ALL {
            assert!(
                out.contains(&format!(
                    "fastllm_rejections_total{{reason=\"{}\"}} 0",
                    why.as_str()
                )),
                "missing {}",
                why.as_str()
            );
        }
    }

    /// The reason per-model metrics live here rather than on the pool: pools
    /// are rebuilt about once a second, and a counter that resets that often
    /// reads to Prometheus as a process restart.
    #[test]
    fn model_counters_survive_a_snapshot_rebuild() {
        let t = Telemetry::new();
        t.rebuild_models(["a", "b"].into_iter());
        t.model("a")
            .unwrap()
            .requests
            .fetch_add(7, Ordering::Relaxed);

        t.rebuild_models(["a", "b", "c"].into_iter());
        assert_eq!(
            t.model("a").unwrap().requests.load(Ordering::Relaxed),
            7,
            "a rebuild must carry the live counters forward"
        );
        assert!(t.model("c").is_some(), "and pick up the new model");
    }

    #[test]
    fn a_model_that_leaves_the_snapshot_stops_being_reported() {
        let t = Telemetry::new();
        t.rebuild_models(["gone"].into_iter());
        t.rebuild_models(["kept"].into_iter());
        assert!(t.model("gone").is_none());

        let mut out = String::new();
        t.render(&mut out);
        assert!(!out.contains("model=\"gone\""));
        assert!(out.contains("fastllm_model_requests_total{model=\"kept\"} 0"));
    }

    /// An unknown model must not create a metrics entry: that is the path a
    /// caller controls, and it is exactly how a metrics endpoint acquires
    /// unbounded cardinality.
    #[test]
    fn an_unknown_model_gets_no_entry() {
        let t = Telemetry::new();
        t.rebuild_models(["known"].into_iter());
        assert!(t.model("../../etc/passwd").is_none());
        assert_eq!(t.models.load().len(), 1);
    }
}
