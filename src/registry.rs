//! Backend inventory: who is serving what, and how loaded are they.
//!
//! A [`Registry`] is immutable once built. Reloading the config builds a fresh
//! one and swaps it in atomically, so request handling never takes a lock to
//! read the routing table. Live counters (in-flight, health) live behind atomics
//! on [`Backend`], which is shared by `Arc` and therefore survives a swap when
//! the same backend appears in both generations.

use anyhow::{Context, Result};
use hyper::header::{HeaderName, HeaderValue};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::FileConfig;
use crate::protocol::{anthropic, Protocol};
use crate::snapshot::{BackendDef, Snapshot};
use sha2::{Digest, Sha256};

/// Milliseconds since the epoch, the single clock the engine-load readings and
/// the routing check that consults them both read.
///
/// Wall-clock rather than `Instant` because the two callers live in different
/// tasks and only need to compare *ages*, and because a monotonic instant
/// cannot be stored in the `AtomicU64` the reading shares with the counter.
#[inline]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Stable identifier for a backend across config reloads.
///
/// The prefix-affinity cache stores these rather than array indices, so a
/// reload that reorders or resizes the model list does not silently
/// re-point warm prefixes at the wrong node.
pub type BackendUid = u32;

/// Assigns a stable [`BackendUid`] to each distinct backend *configuration*.
///
/// Entries are never removed and a uid is never reused, because the affinity
/// cache holds uids for warm prefixes: recycling one onto a different backend
/// would silently misroute them. Reloads that churn the model set therefore
/// grow the table monotonically — at ~2^32 distinct backends per process
/// lifetime the ceiling is unreachable in practice, and a few dozen bytes per
/// backend ever seen is the price of never misrouting.
///
/// The key covers **everything that changes how the backend is called**, not
/// just where it lives. It used to be `(api_base, upstream_model)` alone,
/// which meant a reload that rotated a backend's API key kept serving with
/// the old one: `Registry::build_from_entries` carries the live `Backend`
/// object across reloads to preserve in-flight counts and health, so an
/// unchanged uid meant unchanged credentials, forever, with the admin API
/// cheerfully reporting the new value. Folding the credential and the
/// protocol into the identity means a configuration change produces a new
/// object, while requests still in flight against the old one keep the object
/// whose in-flight counter they will decrement.
#[derive(Default)]
pub struct Interner {
    inner: Mutex<InternerState>,
}

#[derive(Default)]
struct InternerState {
    map: HashMap<String, BackendUid>,
    /// Tracked separately from `map.len()` so uids stay unique regardless of
    /// what the map does.
    next: u64,
}

impl Interner {
    pub fn intern(&self, api_base: &str, def: &BackendDef) -> Result<BackendUid> {
        // The credential is hashed rather than stored: this map lives for the
        // process's lifetime and is never cleared, and a long-lived plaintext
        // copy of every key ever configured is not something to keep around
        // for the sake of a cache key.
        let key_digest = def.api_key.as_deref().map(|k| {
            let mut hasher = Sha256::new();
            hasher.update(k.as_bytes());
            hex::encode(hasher.finalize())
        });
        let key = format!(
            "{api_base}|{}|{}|{}|{}|{}|{}",
            def.upstream_model,
            def.protocol.as_str(),
            def.auth_header,
            def.auth_scheme.as_deref().unwrap_or(""),
            key_digest.as_deref().unwrap_or(""),
            def.default_max_tokens.unwrap_or(0),
        );
        let mut state = self.inner.lock();
        if let Some(uid) = state.map.get(&key) {
            return Ok(*uid);
        }
        let uid: BackendUid = state
            .next
            .try_into()
            .context("more than 4294967295 distinct backends seen over this process's lifetime")?;
        state.next += 1;
        state.map.insert(key, uid);
        Ok(uid)
    }
}

/// One upstream inference server serving one model.
#[derive(Debug)]
pub struct Backend {
    pub uid: BackendUid,
    /// Base URL without trailing slash, e.g. `http://10.0.0.1:8000/v1`.
    pub api_base: String,
    /// Model name to put in the request body sent upstream.
    pub upstream_model: String,
    /// Ready-made auth (and protocol-constant) headers, built once instead of
    /// formatted and re-validated on every request. Empty when the upstream
    /// needs no key and its protocol demands no constants.
    ///
    /// A list rather than a single `Authorization` value because the header
    /// *name* varies by provider — Gemini reads `x-goog-api-key`, Anthropic
    /// `x-api-key` plus a mandatory `anthropic-version` — and because
    /// pre-building them keeps every per-request cost identical to what a
    /// single hardcoded header cost before.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Wire format this upstream speaks. `OpenAi` is passthrough: the request
    /// body is forwarded unread and the response is never parsed.
    pub protocol: Protocol,
    /// `max_tokens` to supply when the request omits one and the protocol
    /// requires it. See `crate::protocol::TranslateError::MissingMaxTokens`.
    pub default_max_tokens: Option<u32>,
    /// Which `model_backends` row this is, carried through so a usage event
    /// can name the attachment that served and be priced at that provider's
    /// rate. Never interpreted here.
    pub backend_id: Option<uuid::Uuid>,
    /// Per-backend upstream timeout override (seconds). `None` means the
    /// global `--upstream-timeout` applies.
    pub upstream_timeout_seconds: Option<u64>,
    /// Input plus output price per million tokens, or `None` when this
    /// backend is unpriced. Pre-added at build time so a caller comparing
    /// cost reads one number per candidate instead of two.
    price_per_mtok: Option<i64>,
    input_price_per_mtok: Option<i64>,
    output_price_per_mtok: Option<i64>,

    healthy: AtomicBool,
    consecutive_failures: AtomicU32,
    /// Consecutive upstream headers timeouts — tracked separately from probe
    /// failures because a timeout after body dispatch means the upstream is
    /// slow or stalled, not unreachable. The rate of timeouts drives the same
    /// unhealthy-after path so a backend that consistently times out is
    /// ejected before every subsequent request burns the full timeout budget.
    consecutive_timeouts: AtomicU32,
    /// Consecutive stall samples: `running > 0` with every engine token
    /// counter frozen since the previous scrape. Deliberately separate from
    /// `consecutive_failures`, which a passing health probe resets — and a
    /// deadlocked vLLM answers `GET /v1/models` just fine, so sharing the
    /// counter would let the probe wipe the stall evidence every sweep.
    consecutive_stalls: AtomicU32,
    inflight: AtomicUsize,
    /// What the engine itself says is in flight, and when it said so.
    ///
    /// `inflight` above counts what *this replica* sent, which is the wrong
    /// number for a ceiling: two proxies each admit up to the limit, so a
    /// `max_inflight_per_backend` of 8 spills at about 16, and anything
    /// reaching the engine another way is invisible. The engine counts once,
    /// for everybody.
    ///
    /// Filled by `crate::engine_scrape`, never on the request path.
    /// `engine_at` is a `now_ms` reading and zero means "never read",
    /// which is how a backend with no `/metrics` — every hosted one — stays
    /// on the local count instead of pretending to be idle.
    engine_inflight: AtomicUsize,
    /// Prefix-cache hit rate as of the last scrape, in per-mille so it fits an
    /// integer atomic; `u32::MAX` means "not reported".
    ///
    /// `cache-affinity` routes on the assumption that a backend still holds
    /// the prefix it was chosen for, and nothing verified that. A backend that
    /// restarted keeps winning the same sessions while re-prefilling every
    /// turn — the exact cost the policy exists to avoid, and invisible.
    prefix_hit_permille: AtomicUsize,
    /// Lookups the engine had served at the last scrape. A *decrease* means
    /// the process restarted and its prefix cache is cold, which is the signal
    /// that the affinity table is now pointing at nothing.
    prefix_queries: AtomicUsize,
    /// The context window this engine last said it accepts. 0 is "it has not
    /// said".
    ///
    /// Live, because the database cannot know: `--max-model-len` is chosen
    /// when the engine starts, so a restart changes it without anything
    /// touching a row. A stale figure is not merely a misreport — routing
    /// demotes a model whose window is smaller than the prompt, so it either
    /// sends an oversized prompt to an engine that will reject it, or stops
    /// routing to one that grew.
    engine_context_length: AtomicUsize,
    engine_at: AtomicU64,
    /// Last engine metrics snapshot: when the reading was taken (milliseconds
    /// since epoch) and the counters that the stall detector compares against.
    engine_last_ts: AtomicU64,
    engine_last_prompt: AtomicU64,
    engine_last_gen: AtomicU64,
    engine_last_kv_tokens: AtomicU32,
    requests_total: AtomicU64,
    errors_total: AtomicU64,
    /// Exponentially weighted mean whole-request latency, in microseconds.
    ///
    /// One `AtomicU64` rather than reading the histogram beside it, because
    /// this is read *per request* by `Policy::LowestLatency` and the
    /// histogram answers only by summing nineteen buckets. That is free for a
    /// Prometheus scrape and not free on the routing path, once per candidate
    /// backend.
    ///
    /// Zero means "nothing measured yet", which the router treats as
    /// unknown-and-therefore-eligible rather than as instantaneous — a fresh
    /// backend that read as 0 µs would win every comparison and take the
    /// whole pool until its first request completed.
    latency_ewma_us: AtomicU64,
    /// Whole-request wall time for requests this backend served.
    ///
    /// Lives here rather than in the telemetry module's per-model map because
    /// backends already survive a snapshot rebuild — the registry carries the
    /// live object forward by uid — and because the question it answers is
    /// per replica. A per-model p99 rising tells you a model got slow; this
    /// tells you which of its replicas did.
    pub duration: crate::telemetry::Histogram,
}

impl Backend {
    fn new(uid: BackendUid, api_base: String, def: &BackendDef) -> Result<Self> {
        let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        if let Some(key) = def.api_key.as_deref() {
            let name = HeaderName::from_bytes(def.auth_header.to_ascii_lowercase().as_bytes())
                .with_context(|| {
                    format!(
                        "auth_header {:?} for {api_base} is not a valid header name",
                        def.auth_header
                    )
                })?;
            let value = match def.auth_scheme.as_deref() {
                Some(scheme) if !scheme.is_empty() => format!("{scheme} {key}"),
                // Raw key, no prefix: what `x-api-key`/`x-goog-api-key` want.
                _ => key.to_string(),
            };
            headers.push((
                name,
                HeaderValue::from_str(&value).with_context(|| {
                    format!("api_key for {api_base} is not a valid header value")
                })?,
            ));
        }
        // Protocol constants the operator must not have to know about, and
        // could get wrong: a mismatched `anthropic-version` changes response
        // shapes underneath the translator.
        if def.protocol == Protocol::Anthropic {
            headers.push((
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(anthropic::API_VERSION),
            ));
        }
        Ok(Self {
            uid,
            api_base,
            upstream_model: def.upstream_model.clone(),
            headers,
            protocol: def.protocol,
            default_max_tokens: def.default_max_tokens,
            backend_id: def.backend_id,
            upstream_timeout_seconds: def.upstream_timeout_seconds,
            // Either figure alone is enough to call a backend priced: a
            // provider that charges for input and nothing for output is a
            // real arrangement, and reading the missing half as "unknown"
            // would make the whole backend unpriced.
            price_per_mtok: match (def.input_price_per_mtok, def.output_price_per_mtok) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            },
            input_price_per_mtok: def.input_price_per_mtok,
            output_price_per_mtok: def.output_price_per_mtok,
            // Optimistic: a backend serves traffic until a health check says
            // otherwise. Starting unhealthy would blackhole every request in
            // the window before the first sweep completes.
            healthy: AtomicBool::new(true),
            consecutive_failures: AtomicU32::new(0),
            consecutive_timeouts: AtomicU32::new(0),
            consecutive_stalls: AtomicU32::new(0),
            inflight: AtomicUsize::new(0),
            engine_inflight: AtomicUsize::new(0),
            prefix_hit_permille: AtomicUsize::new(usize::MAX),
            prefix_queries: AtomicUsize::new(0),
            engine_context_length: AtomicUsize::new(0),
            engine_at: AtomicU64::new(0),
            engine_last_ts: AtomicU64::new(0),
            engine_last_prompt: AtomicU64::new(0),
            engine_last_gen: AtomicU64::new(0),
            engine_last_kv_tokens: AtomicU32::new(0),
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            latency_ewma_us: AtomicU64::new(0),
            duration: crate::telemetry::Histogram::new(),
        })
    }

    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// What this backend charges per million tokens, in and out together.
    /// `None` is unpriced, which is a different thing from free.
    #[inline]
    pub fn price_per_mtok(&self) -> Option<i64> {
        self.price_per_mtok
    }

    /// The two figures separately, for costing a request whose prompt and
    /// generation are billed at different rates. `None` is unpriced.
    #[inline]
    pub fn prices(&self) -> Option<(i64, i64)> {
        self.price_per_mtok?;
        Some((
            self.input_price_per_mtok.unwrap_or(0),
            self.output_price_per_mtok.unwrap_or(0),
        ))
    }

    #[inline]
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Keep the prefix-cache reading, and notice when the cache went away.
    ///
    /// A cumulative counter that goes *down* means the engine process
    /// restarted, so whatever prefixes it held are gone. The affinity table
    /// still points at it, and would keep pointing at it while every turn
    /// re-prefilled from nothing — 85 minutes of that happened on this fleet
    /// with no operator-visible signal. Zeroing the stored rate is what lets
    /// `prefix_cache_hit_rate` report "cold" rather than a stale 96%.
    fn record_prefix_cache(&self, load: &crate::engine_metrics::EngineLoad) {
        let Some(queries) = load.prefix_cache_queries_total else {
            return;
        };
        let previous = self
            .prefix_queries
            .swap(queries as usize, Ordering::Relaxed);
        if (queries as usize) < previous {
            // Restarted: nothing it reports yet describes the cache the
            // affinity table was built against.
            self.prefix_hit_permille
                .store(usize::MAX, Ordering::Relaxed);
            return;
        }
        match load.prefix_cache_hit_rate() {
            Some(rate) => self.prefix_hit_permille.store(
                (rate * 1000.0).round().clamp(0.0, 1000.0) as usize,
                Ordering::Relaxed,
            ),
            None => self
                .prefix_hit_permille
                .store(usize::MAX, Ordering::Relaxed),
        }
    }

    /// Record the context window an engine reported, from the health probe's
    /// own response — no extra request: that body was being drained and
    /// discarded.
    pub fn record_context_length(&self, len: u64) {
        self.engine_context_length
            .store(len as usize, Ordering::Relaxed);
    }

    /// What this engine says it accepts, or `None` if it never has.
    pub fn engine_context_length(&self) -> Option<u64> {
        match self.engine_context_length.load(Ordering::Relaxed) {
            0 => None,
            len => Some(len as u64),
        }
    }

    /// The engine's prefix-cache hit rate as of the last scrape, 0.0–1.0.
    ///
    /// `None` when the engine does not report it, when it has served no
    /// lookups, or when it restarted since the last scrape — all three are
    /// "unknown", and a zero would read as "affinity is failing" when it may
    /// simply be a backend nobody has used yet.
    pub fn prefix_cache_hit_rate(&self) -> Option<f32> {
        match self.prefix_hit_permille.load(Ordering::Relaxed) {
            usize::MAX => None,
            permille => Some(permille as f32 / 1000.0),
        }
    }

    /// How stale an engine reading may be and still be believed.
    ///
    /// Generous next to the scrape interval, and deliberately so: the cost of
    /// believing a two-second-old count is that a burst is noticed slightly
    /// late, while the cost of discarding it is falling back to a per-replica
    /// number that is wrong by a constant factor. A backend that stops
    /// answering `/metrics` for longer than this stops being special.
    const ENGINE_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(10);

    /// Record what the engine reported. Called by the scraper, never by a
    /// request.
    pub fn record_engine_inflight(&self, load: &crate::engine_metrics::EngineLoad, now_ms: u64) {
        self.engine_inflight
            .store((load.running + load.waiting) as usize, Ordering::Relaxed);
        self.engine_at.store(now_ms.max(1), Ordering::Relaxed);
        self.engine_last_ts.store(now_ms.max(1), Ordering::Relaxed);
        self.engine_last_prompt
            .store(load.prompt_tokens_total, Ordering::Relaxed);
        self.engine_last_gen
            .store(load.generation_tokens_total, Ordering::Relaxed);
        self.engine_last_kv_tokens
            .store(load.kv_cache_tokens.unwrap_or(0), Ordering::Relaxed);
        self.record_prefix_cache(load);
    }

    /// Check whether the engine has stalled since the last metrics scrape.
    ///
    /// A stall is one where the engine reports `running > 0` (has work) but
    /// nothing has progressed: both token counters and KV cache tokens are
    /// identical to the last reading. Two consecutive stall samples mark the
    /// backend unhealthy through the same path as a dead health probe, so
    /// that the failover loop discovers it and stops burning requests on it.
    ///
    /// Returns `true` if this sample transitioned the backend out of rotation.
    ///
    /// Must be called *before* `record_engine_inflight` overwrites the stored
    /// reading with the current one — recorded first, every scrape would
    /// trivially match itself and the comparison would compare nothing.
    pub fn check_stall(&self, load: &crate::engine_metrics::EngineLoad, threshold: u32) -> bool {
        // No previous scrape to compare against yet.
        if self.engine_last_ts.load(Ordering::Relaxed) == 0 {
            return false;
        }

        // Must be running something to be stalling — an idle engine with
        // frozen counters is just an idle engine. Clear accumulated evidence
        // so a later hang starts from a clean counter.
        if load.running == 0 {
            self.consecutive_stalls.store(0, Ordering::Relaxed);
            return false;
        }

        let frozen = self.engine_last_prompt.load(Ordering::Relaxed) == load.prompt_tokens_total
            && self.engine_last_gen.load(Ordering::Relaxed) == load.generation_tokens_total
            && self.engine_last_kv_tokens.load(Ordering::Relaxed)
                == load.kv_cache_tokens.unwrap_or(0);

        // Any movement — prefill climbing, decode climbing, cache growing —
        // is proof of life, however slow the backend is.
        if !frozen {
            self.consecutive_stalls.store(0, Ordering::Relaxed);
            return false;
        }

        let stalled = self.consecutive_stalls.fetch_add(1, Ordering::Relaxed) + 1 >= threshold;
        if stalled {
            self.eject("engine counters frozen while requests were running");
        }
        stalled
    }

    /// Reset the stall counter, e.g. when a backend leaves and re-enters
    /// rotation and its previous evidence no longer describes it.
    pub fn reset_stall_counter(&self) {
        self.consecutive_stalls.store(0, Ordering::Relaxed);
    }

    /// The count a ceiling should be compared against.
    ///
    /// The engine's when it is fresh, this replica's otherwise. Never a sum:
    /// the engine's number already includes what this replica sent, so adding
    /// them would double-count exactly the requests we know most about.
    pub fn inflight_for_limit(&self, now_ms: u64) -> usize {
        let at = self.engine_at.load(Ordering::Relaxed);
        if at != 0 && now_ms.saturating_sub(at) <= Self::ENGINE_FRESH_FOR.as_millis() as u64 {
            return self.engine_inflight.load(Ordering::Relaxed);
        }
        self.inflight()
    }

    pub fn requests_total(&self) -> u64 {
        self.requests_total.load(Ordering::Relaxed)
    }

    pub fn errors_total(&self) -> u64 {
        self.errors_total.load(Ordering::Relaxed)
    }

    /// Fold one completed request's duration into the latency EWMA.
    ///
    /// α = 1/8, chosen so a backend that degrades is reflected within a
    /// handful of requests but a single slow generation cannot hand the whole
    /// pool to its neighbour. Fixed-point in microseconds — no floats, no
    /// lock, one compare-and-swap that falls back to a plain store.
    ///
    /// Load, compute, store rather than a CAS loop: two requests finishing
    /// together can interleave and one update is lost. That is acceptable
    /// here and a CAS retry is not — this runs on request completion, and an
    /// estimate that is one sample stale is worth strictly less than the
    /// contention avoided.
    pub fn note_latency_us(&self, us: u64) {
        let prev = self.latency_ewma_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            us
        } else {
            // prev * 7/8 + us * 1/8, in integers.
            prev - (prev >> 3) + (us >> 3)
        };
        self.latency_ewma_us.store(next, Ordering::Relaxed);
    }

    /// The EWMA, or `None` when this backend has completed nothing yet.
    ///
    /// `None` rather than 0 so a caller cannot accidentally rank an unmeasured
    /// backend as the fastest thing in the pool.
    pub fn latency_us(&self) -> Option<u64> {
        match self.latency_ewma_us.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    /// Increment the error counter. Does not affect health — used for
    /// retryable errors where the backend may still be serving.
    pub fn note_error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the consecutive timeout counter and, if it exceeds the
    /// threshold, mark the backend unhealthy.
    ///
    /// A headers timeout after body dispatch is not a "connection refused" —
    /// the upstream accepted the request but did not produce a response within
    /// the configured budget. The caller has already sent the full body and
    /// the timeout counts against the pool's effective capacity. If a backend
    /// repeatedly times out it is being treated as unhealthy through the same
    /// path as a dead probe, so that the failover loop eventually discovers
    /// it and stops burning requests on it.
    ///
    /// The error count is *not* incremented here. A timeout is one error, and
    /// the caller has already recorded it with `note_error`; counting it again
    /// made `errors_total` — and every error-rate alert built on it — read
    /// double the real rate for a timing-out backend.
    pub fn note_timeout(&self, threshold: u32) {
        let failures = self.consecutive_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= threshold {
            self.eject("consecutive upstream header timeouts");
            self.consecutive_timeouts.store(0, Ordering::Relaxed);
        }
    }

    /// Reconsider a local ejection the rest of the fleet disagrees with.
    ///
    /// Returns true if this cleared one, so the caller can log a transition
    /// that is otherwise invisible.
    ///
    /// Backend health is per replica and in memory, so ten proxies hold ten
    /// independent opinions and nothing reconciles them. That is usually
    /// right — a proxy that cannot reach a backend should stop using it — but
    /// it has no way to tell "the backend is down" from "I am wrong", and a
    /// wrong one is invisible: the replica keeps taking that model's traffic,
    /// answers 502, and stays Ready because its other backends are fine. One
    /// replica held a backend ejected for hours while nine served it and the
    /// engine answered a probe in 13ms.
    ///
    /// So this does not force the backend healthy — it *withdraws the
    /// verdict*, clearing the ejection and its counters so the next local
    /// probe decides again from scratch. If this replica genuinely cannot
    /// reach the backend, its own sweep re-ejects it within one probe
    /// interval and nothing has been lost. That self-limiting shape is why
    /// this cannot resurrect a dead backend: the fleet only ever buys a
    /// re-examination, never a conclusion.
    pub fn reconsider(&self) -> bool {
        if self.healthy.load(Ordering::Relaxed) {
            return false;
        }
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.consecutive_timeouts.store(0, Ordering::Relaxed);
        self.consecutive_stalls.store(0, Ordering::Relaxed);
        self.healthy.store(true, Ordering::Relaxed);
        true
    }

    /// Take this backend out of rotation, saying why.
    ///
    /// Every path to "unhealthy" goes through here so that none of them is
    /// silent. Two of them were, and it cost a user report to notice: a
    /// backend left rotation, requests for its model answered 502, and the
    /// only trace in the log was the *recovery* — "back in rotation" with
    /// nothing before it. An operator reading that sees a backend healing
    /// from an illness the log never mentioned.
    ///
    /// Logged at warn because it changes what the replica will serve, and
    /// only on the transition: a backend that is already out stays out
    /// quietly rather than repeating itself every probe interval.
    fn eject(&self, reason: &str) {
        if !self.healthy.swap(false, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            backend = %self.api_base,
            model = %self.upstream_model,
            reason,
            "backend out of rotation"
        );
    }

    /// Reset consecutive timeouts on a successful response.
    pub fn reset_timeout_count(&self) {
        self.consecutive_timeouts.store(0, Ordering::Relaxed);
    }

    /// Record a successful health probe.
    pub fn mark_probe_ok(&self) -> bool {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        !self.healthy.swap(true, Ordering::Relaxed)
    }

    /// Record a failed health probe. Returns true if this transitioned the
    /// backend out of rotation.
    pub fn mark_probe_failed(&self, threshold: u32) -> bool {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= threshold {
            return self.healthy.swap(false, Ordering::Relaxed);
        }
        false
    }

    /// URL for a sub-path of the OpenAI API, e.g. `/chat/completions`.
    pub fn url_for(&self, subpath: &str) -> String {
        format!("{}{}", self.api_base, subpath)
    }
}

/// Increments a backend's in-flight count for as long as it is alive.
///
/// Held by the response body wrapper, so the count only drops when the last
/// token has been streamed to the client — not when the upstream headers
/// arrive. Getting this wrong makes every streaming backend look idle and
/// collapses least-loaded routing into round-robin.
pub struct InflightGuard(Arc<Backend>);

impl InflightGuard {
    pub fn acquire(backend: Arc<Backend>) -> Self {
        backend.inflight.fetch_add(1, Ordering::Relaxed);
        backend.requests_total.fetch_add(1, Ordering::Relaxed);
        Self(backend)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The set of backends serving one backend model, and how to choose between
/// them.
///
/// The policy lives here rather than only on the `Router` because it is a
/// property of the pool, not of the process: a model served by two identical
/// local replicas wants prefix affinity, while one fronted by three hosted
/// providers of differing speed wants lowest-latency, and a deployment
/// routinely has both. `None` means "whatever the deployment was started
/// with" (`--policy`), which is what every pool meant before this existed.
#[derive(Debug, Default)]
pub struct PoolInner {
    pub backends: Vec<Arc<Backend>>,
    pub policy: Option<crate::router::Policy>,
}

impl std::ops::Deref for PoolInner {
    type Target = [Arc<Backend>];

    // So a pool still reads as the list of backends it mostly is: `pool.iter()`,
    // `pool.len()` and indexing all keep working, and only code that cares
    // about the policy has to know there is more here than a Vec.
    fn deref(&self) -> &Self::Target {
        &self.backends
    }
}

pub type Pool = Arc<PoolInner>;

/// Immutable routing table.
#[derive(Default)]
pub struct Registry {
    pools: HashMap<String, Pool>,
    /// Every distinct backend, for health sweeps and metrics.
    all: Vec<Arc<Backend>>,
    /// Model name to declared context window. Absent means undeclared, which
    /// is a third state routing must handle — see `ModelDef::context_length`.
    context_length: HashMap<String, u64>,
}

impl Registry {
    /// Build a registry from a parsed config.
    ///
    /// `previous` is consulted so that a backend which survives a reload keeps
    /// its live counters and health state instead of being reset to optimistic.
    pub fn build(
        cfg: &FileConfig,
        interner: &Interner,
        previous: Option<&Registry>,
    ) -> Result<Self> {
        // Rejected here rather than defaulted: `protocol: anthropc` silently
        // becoming an OpenAI backend pointed at Anthropic produces a stream of
        // upstream 400s that look like the provider's fault.
        for entry in &cfg.model_list {
            if !entry.litellm_params.protocol_is_valid() {
                anyhow::bail!(
                    "model {:?}: protocol {:?} is not one of openai, anthropic, gemini",
                    entry.model_name,
                    entry.litellm_params.protocol.as_deref().unwrap_or_default()
                );
            }
        }
        let entries = cfg.model_list.iter().map(|entry| {
            let api_base = entry
                .litellm_params
                .api_base
                .trim_end_matches('/')
                .to_string();
            (
                entry.model_name.clone(),
                api_base,
                BackendDef {
                    upstream_model: entry.litellm_params.upstream_model(&entry.model_name),
                    api_key: entry.litellm_params.effective_api_key(),
                    protocol: entry.litellm_params.protocol_or_default(),
                    auth_header: entry
                        .litellm_params
                        .auth_header
                        .clone()
                        .unwrap_or_else(|| "authorization".to_string()),
                    auth_scheme: entry.litellm_params.auth_scheme_or_default(),
                    default_max_tokens: entry.litellm_params.default_max_tokens,
                    ..Default::default()
                },
            )
        });
        Self::build_from_entries(entries, interner, previous)
    }

    /// Build a registry straight from a control-plane (or `File`-derived)
    /// [`Snapshot`] rather than the YAML config.
    ///
    /// `Snapshot::models` already carries exactly what a backend needs,
    /// because `FileSource` builds
    /// one from the same YAML `build` reads and the control plane builds one
    /// from Postgres. Routing from the snapshot means there is one place that
    /// turns model data into the routing table regardless of where the data
    /// came from, which is what lets [`spawn_poller`](crate::source::spawn_poller)
    /// keep both the snapshot and the registry current from a single fetch.
    pub fn build_from_snapshot(
        snapshot: &Snapshot,
        interner: &Interner,
        previous: Option<&Registry>,
    ) -> Result<Self> {
        let entries = snapshot.models.iter().flat_map(|model| {
            model.backends.iter().map(move |b| {
                (
                    model.name.clone(),
                    b.api_base.trim_end_matches('/').to_string(),
                    b.clone(),
                )
            })
        });
        let policies = snapshot
            .models
            .iter()
            .filter_map(|m| m.policy.map(|p| (m.name.clone(), p)))
            .collect();
        let mut registry =
            Self::build_from_entries_with_policies(entries, &policies, interner, previous)?;
        // Declared context windows, which this field's doc comment has always
        // claimed were filled in here and never were: the map stayed empty in
        // every production build, so `Registry::context_length` answered
        // `None` for every model and the context-window fallback in
        // `routing::candidates` — a model whose window provably cannot hold
        // the request is demoted rather than tried first — could not fire at
        // all. The column, the admin API field and the routing code were all
        // present and correct; only the wiring between them was missing.
        registry.context_length = snapshot
            .models
            .iter()
            .filter_map(|m| m.context_length.map(|len| (m.name.clone(), len)))
            .collect();
        Ok(registry)
    }

    /// Tokens a model can accept, or `None` when nobody has declared it.
    ///
    /// Lives here rather than only on the snapshot because routing asks the
    /// registry, not the snapshot, and the alternative was threading a second
    /// lookup through every call site that already has a `&Registry`.
    pub fn context_length(&self, model: &str) -> Option<u64> {
        // What the engines currently say wins over what was recorded. The
        // stored figure is a snapshot of a value chosen at engine start, so a
        // restart with a different `--max-model-len` leaves it describing a
        // process that no longer exists — and routing demotes a model whose
        // window is smaller than the prompt, so believing it either sends an
        // oversized prompt to an engine that will reject it or stops routing
        // to one that grew.
        //
        // The smallest across the model's backends, because any of them may
        // serve the request and the figure has to hold for whichever does.
        // Backends that have not said are skipped rather than counted as
        // zero, which would make one silent engine shrink the whole pool.
        let live = self
            .pool(model)
            .and_then(|pool| pool.iter().filter_map(|b| b.engine_context_length()).min());
        live.or_else(|| self.context_length.get(model).copied())
    }

    /// Declare context windows on a registry built from YAML, which has no
    /// syntax for them. Test-only: the real path is a control-plane snapshot.
    #[cfg(test)]
    pub fn set_context_lengths_for_test(&mut self, lengths: HashMap<String, u64>) {
        self.context_length = lengths;
    }

    fn build_from_entries(
        entries: impl Iterator<Item = (String, String, BackendDef)>,
        interner: &Interner,
        previous: Option<&Registry>,
    ) -> Result<Self> {
        Self::build_from_entries_with_policies(entries, &HashMap::new(), interner, previous)
    }

    fn build_from_entries_with_policies(
        entries: impl Iterator<Item = (String, String, BackendDef)>,
        policies: &HashMap<String, crate::router::Policy>,
        interner: &Interner,
        previous: Option<&Registry>,
    ) -> Result<Self> {
        let mut pools: HashMap<String, Vec<Arc<Backend>>> = HashMap::new();
        let mut by_uid: HashMap<BackendUid, Arc<Backend>> = HashMap::new();

        for (model_name, api_base, def) in entries {
            let uid = interner.intern(&api_base, &def)?;

            // Reuse the live object when we already made one this pass, or when
            // the previous generation had it — preserving in-flight and health.
            let backend = if let Some(existing) = by_uid.get(&uid) {
                Arc::clone(existing)
            } else {
                let carried = previous
                    .and_then(|p| p.all.iter().find(|b| b.uid == uid))
                    .cloned();
                let backend = match carried {
                    // Same uid means byte-identical configuration (see
                    // `Interner`), so carrying the live object forward carries
                    // no stale settings with it — only the counters and health
                    // state it is kept for.
                    Some(live) => live,
                    None => Arc::new(Backend::new(uid, api_base.clone(), &def)?),
                };
                by_uid.insert(uid, Arc::clone(&backend));
                backend
            };

            let pool = pools.entry(model_name).or_default();
            // The same backend can legitimately be listed twice for one model
            // (e.g. an alias resolving onto it); only route to it once.
            if !pool.iter().any(|b: &Arc<Backend>| b.uid == uid) {
                pool.push(backend);
            }
        }

        let mut all: Vec<Arc<Backend>> = by_uid.into_values().collect();
        all.sort_by_key(|b| b.uid);

        Ok(Self {
            pools: pools
                .into_iter()
                .map(|(name, backends)| {
                    let policy = policies.get(&name).copied();
                    (name, Arc::new(PoolInner { backends, policy }))
                })
                .collect(),
            all,
            // Filled by `build_from_snapshot`; the YAML path has nowhere to
            // declare a context window, so it stays empty and every model
            // there reads as undeclared.
            context_length: HashMap::new(),
        })
    }

    pub fn pool(&self, model_name: &str) -> Option<&Pool> {
        self.pools.get(model_name)
    }

    pub fn model_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.pools.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    pub fn backends(&self) -> &[Arc<Backend>] {
        &self.all
    }

    pub fn healthy_count(&self) -> usize {
        self.all.iter().filter(|b| b.is_healthy()).count()
    }

    /// Whether `model_name` has a pool with at least one backend currently in
    /// rotation.
    ///
    /// Used by frontend-model target selection (`crate::routing`) to decide
    /// whether a target is "unhealthy or saturated" enough to fall through to
    /// the next one in its chain. A model with no pool at all (misconfigured
    /// target, or a name that does not exist) counts the same as one with
    /// every backend down: nothing here can serve the request.
    pub fn pool_has_healthy(&self, model_name: &str) -> bool {
        self.pools
            .get(model_name)
            .is_some_and(|pool| pool.iter().any(|b| b.is_healthy()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> FileConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    const TWO_REPLICAS: &str = r#"
model_list:
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.1:8000/v1
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.2:8000/v1
"#;

    #[test]
    fn same_model_name_forms_one_pool() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        assert_eq!(reg.pool("Qwen/Qwen3-1.7B").unwrap().len(), 2);
        assert_eq!(reg.backends().len(), 2);
    }

    #[test]
    fn duplicate_entry_is_not_routed_to_twice() {
        let dup = format!(
            "{TWO_REPLICAS}{}",
            r#"  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.1:8000/v1
"#
        );
        let reg = Registry::build(&config(&dup), &Interner::default(), None).unwrap();
        assert_eq!(reg.pool("Qwen/Qwen3-1.7B").unwrap().len(), 2);
    }

    fn load(
        running: u32,
        prompt: u64,
        gen: u64,
        kv: Option<u32>,
    ) -> crate::engine_metrics::EngineLoad {
        crate::engine_metrics::EngineLoad {
            prefix_cache_queries_total: None,
            prefix_cache_hits_total: None,
            running,
            waiting: 0,
            kv_cache: None,
            prompt_tokens_total: prompt,
            generation_tokens_total: gen,
            kv_cache_tokens: kv,
        }
    }

    #[test]
    fn stall_needs_two_frozen_samples_and_a_previous_scrape() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        let l = load(2, 100, 50, Some(10));
        // Nothing scraped yet: comparison has no baseline.
        assert!(!b.check_stall(&l, 2));
        b.record_engine_inflight(&l, 1);
        // First frozen sample: below the threshold, still in rotation.
        assert!(!b.check_stall(&l, 2));
        assert!(b.is_healthy());
        // Second: ejected.
        assert!(b.check_stall(&l, 2));
        assert!(!b.is_healthy());
    }

    /// The signal `cache-affinity` never had.
    ///
    /// A hit rate is only meaningful once the engine has served lookups, and a
    /// counter that goes backwards means the process restarted and the cache
    /// the affinity table was built against is gone. Reporting a stale 96% at
    /// that moment is worse than reporting nothing: it is the number an
    /// operator would use to conclude affinity is working.
    #[test]
    fn a_restarted_engine_stops_claiming_its_old_hit_rate() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);

        // Nothing reported: unknown, not zero.
        let mut load = load(0, 0, 0, None);
        b.record_engine_inflight(&load, 1);
        assert_eq!(b.prefix_cache_hit_rate(), None, "no counters means unknown");

        // Warm: 96 hits in 100 lookups.
        load.prefix_cache_queries_total = Some(100);
        load.prefix_cache_hits_total = Some(96);
        b.record_engine_inflight(&load, 2);
        let warm = b
            .prefix_cache_hit_rate()
            .expect("a rate once it has lookups");
        assert!((warm - 0.96).abs() < 0.002, "got {warm}");

        // Restarted: the counter fell, so the cache is cold and the old rate
        // describes a process that no longer exists.
        load.prefix_cache_queries_total = Some(3);
        load.prefix_cache_hits_total = Some(0);
        b.record_engine_inflight(&load, 3);
        assert_eq!(
            b.prefix_cache_hit_rate(),
            None,
            "a counter going backwards means restarted, and the old rate is a lie"
        );

        // And it recovers once the new process has served enough to speak for
        // itself.
        load.prefix_cache_queries_total = Some(50);
        load.prefix_cache_hits_total = Some(25);
        b.record_engine_inflight(&load, 4);
        let recovered = b.prefix_cache_hit_rate().expect("reporting again");
        assert!((recovered - 0.5).abs() < 0.002, "got {recovered}");
    }

    /// Zero lookups is unknown, not 0%. An idle backend showing 0% sends an
    /// operator hunting a cache problem that is not there.
    #[test]
    fn no_lookups_is_unknown_rather_than_zero_percent() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        let mut load = load(0, 0, 0, None);
        load.prefix_cache_queries_total = Some(0);
        load.prefix_cache_hits_total = Some(0);
        b.record_engine_inflight(&load, 1);
        assert_eq!(b.prefix_cache_hit_rate(), None);
    }

    #[test]
    fn any_counter_movement_is_proof_of_life() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        let a = load(2, 100, 50, Some(10));
        b.record_engine_inflight(&a, 1);
        assert!(!b.check_stall(&a, 2)); // one frozen sample banked
                                        // Prefill advanced: the check itself must see the movement and
                                        // wipe the banked evidence — production order is check, then record.
        let moved = load(2, 140, 50, Some(10));
        assert!(!b.check_stall(&moved, 2));
        b.record_engine_inflight(&moved, 2);
        // Frozen again, but the banked sample was wiped by the movement.
        assert!(!b.check_stall(&moved, 2));
        assert!(b.is_healthy());
    }

    #[test]
    fn idle_engine_is_not_stalled() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        let busy = load(2, 100, 50, Some(10));
        b.record_engine_inflight(&busy, 1);
        assert!(!b.check_stall(&busy, 2)); // one sample banked
        assert!(!b.check_stall(&load(0, 100, 50, Some(10)), 2)); // drained: evidence cleared
                                                                 // A later hang restarts from a clean counter.
        assert!(!b.check_stall(&busy, 2));
        assert!(b.is_healthy());
    }

    #[test]
    fn passing_health_probe_does_not_wipe_stall_evidence() {
        // The failure mode of #19: a deadlocked engine still answers
        // GET /v1/models, so the probe must not reset the stall counter.
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        let l = load(2, 100, 50, Some(10));
        b.record_engine_inflight(&l, 1);
        assert!(!b.check_stall(&l, 2));
        b.mark_probe_ok();
        assert!(b.check_stall(&l, 2));
        assert!(!b.is_healthy());
    }

    #[test]
    fn consecutive_timeouts_eject_and_any_answer_clears() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        b.note_timeout(2);
        assert!(b.is_healthy());
        b.reset_timeout_count(); // an answer arrived, streak broken
        b.note_timeout(2);
        assert!(b.is_healthy());
        b.note_timeout(2);
        assert!(!b.is_healthy());
    }

    #[test]
    fn a_timeout_counts_as_one_error_not_two() {
        // The proxy records a timeout with note_error + note_timeout. When
        // both bumped errors_total, every error-rate panel doubled.
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        b.note_error();
        b.note_timeout(2);
        assert_eq!(b.errors_total(), 1);
    }

    #[test]
    fn alias_shares_the_backend_object_with_its_target() {
        let yaml = r#"
model_list:
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.1:8000/v1
  - model_name: gpt-4
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.1:8000/v1
"#;
        let reg = Registry::build(&config(yaml), &Interner::default(), None).unwrap();
        let real = &reg.pool("Qwen/Qwen3-1.7B").unwrap()[0];
        let alias = &reg.pool("gpt-4").unwrap()[0];
        assert!(Arc::ptr_eq(real, alias));
        assert_eq!(alias.upstream_model, "Qwen/Qwen3-1.7B");
        // One physical backend, listed under two client-facing names.
        assert_eq!(reg.backends().len(), 1);
    }

    #[test]
    fn reload_preserves_health_and_inflight() {
        let interner = Interner::default();
        let old = Registry::build(&config(TWO_REPLICAS), &interner, None).unwrap();
        let b = Arc::clone(&old.backends()[0]);
        b.mark_probe_failed(1);
        let _guard = InflightGuard::acquire(Arc::clone(&b));
        assert!(!b.is_healthy());
        assert_eq!(b.inflight(), 1);

        let new = Registry::build(&config(TWO_REPLICAS), &interner, Some(&old)).unwrap();
        let carried = new.backends().iter().find(|x| x.uid == b.uid).unwrap();
        assert!(
            Arc::ptr_eq(carried, &b),
            "reload must carry the live object over"
        );
        assert!(!carried.is_healthy());
        assert_eq!(carried.inflight(), 1);
    }

    #[test]
    fn uid_is_stable_across_reload() {
        let interner = Interner::default();
        let a = Registry::build(&config(TWO_REPLICAS), &interner, None).unwrap();
        let uids_a: Vec<_> = a.backends().iter().map(|b| b.uid).collect();
        // Same two backends, reversed order in the file.
        let reversed = r#"
model_list:
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.2:8000/v1
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.0.0.1:8000/v1
"#;
        let b = Registry::build(&config(reversed), &interner, Some(&a)).unwrap();
        let uids_b: Vec<_> = b.backends().iter().map(|x| x.uid).collect();
        assert_eq!(uids_a, uids_b);
    }

    /// Azure OpenAI and anything else that wants the key in its own header
    /// with no `Bearer` prefix. Both public comparisons of this proxy listed
    /// Azure as unsupported and estimated weeks of work; it is two config
    /// fields, and this pins that so the claim can be made honestly.
    #[test]
    fn a_custom_auth_header_carries_the_raw_key() {
        let azure = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_base: https://example.openai.azure.com/openai/deployments/gpt-4o
      api_key: secret-key
      auth_header: api-key
      auth_scheme: ""
"#;
        let reg = Registry::build(&config(azure), &Interner::default(), None).unwrap();
        let headers = &reg.backends()[0].headers;
        let sent: Vec<(String, String)> = headers
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        assert!(
            sent.iter()
                .any(|(n, v)| n == "api-key" && v == "secret-key"),
            "expected a bare api-key header, got {sent:?}"
        );
        assert!(
            !sent.iter().any(|(n, _)| n == "authorization"),
            "a custom auth header must replace Authorization, not add to it: {sent:?}"
        );
    }

    /// A native backend from a YAML file. Before this, `protocol` was only
    /// expressible through the control plane, so a `File`-mode deployment
    /// could not talk to Anthropic or Gemini at all — and the config silently
    /// produced an OpenAI backend pointed at an endpoint that speaks a
    /// different language.
    #[test]
    fn a_native_backend_can_be_configured_from_yaml() {
        let native = r#"
model_list:
  - model_name: claude
    litellm_params:
      model: claude-sonnet-4-5
      api_base: https://api.anthropic.com/v1
      api_key: sk-ant-x
      protocol: anthropic
      auth_header: x-api-key
      auth_scheme: ""
      default_max_tokens: 4096
"#;
        let reg = Registry::build(&config(native), &Interner::default(), None).unwrap();
        let b = &reg.backends()[0];
        assert_eq!(b.protocol, crate::protocol::Protocol::Anthropic);
        assert_eq!(b.default_max_tokens, Some(4096));
        let sent: Vec<String> = b
            .headers
            .iter()
            .map(|(n, _)| n.as_str().to_string())
            .collect();
        assert!(sent.iter().any(|n| n == "x-api-key"), "{sent:?}");
        // The translator needs this and an operator should not have to know
        // it exists; the registry adds it for anthropic backends.
        assert!(sent.iter().any(|n| n == "anthropic-version"), "{sent:?}");
    }

    #[test]
    fn a_misspelled_protocol_is_refused_at_startup() {
        let typo = r#"
model_list:
  - model_name: claude
    litellm_params:
      model: claude-sonnet-4-5
      api_base: https://api.anthropic.com/v1
      protocol: anthropc
"#;
        let err = match Registry::build(&config(typo), &Interner::default(), None) {
            Err(e) => e,
            Ok(_) => panic!("a protocol nobody implements must not default to openai"),
        };
        assert!(err.to_string().contains("anthropc"), "{err}");
    }

    #[test]
    fn guard_releases_inflight_on_drop() {
        let reg = Registry::build(&config(TWO_REPLICAS), &Interner::default(), None).unwrap();
        let b = Arc::clone(&reg.backends()[0]);
        {
            let _g = InflightGuard::acquire(Arc::clone(&b));
            assert_eq!(b.inflight(), 1);
        }
        assert_eq!(b.inflight(), 0);
        assert_eq!(b.requests_total(), 1);
    }
}
