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

    healthy: AtomicBool,
    consecutive_failures: AtomicU32,
    inflight: AtomicUsize,
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
            // Optimistic: a backend serves traffic until a health check says
            // otherwise. Starting unhealthy would blackhole every request in
            // the window before the first sweep completes.
            healthy: AtomicBool::new(true),
            consecutive_failures: AtomicU32::new(0),
            inflight: AtomicUsize::new(0),
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

    #[inline]
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
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

    pub fn note_error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
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

/// The set of backends serving one client-facing model name.
pub type Pool = Arc<Vec<Arc<Backend>>>;

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
                    auth_scheme: entry.litellm_params.auth_scheme.clone(),
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
        Self::build_from_entries(entries, interner, previous)
    }

    /// Tokens a model can accept, or `None` when nobody has declared it.
    ///
    /// Lives here rather than only on the snapshot because routing asks the
    /// registry, not the snapshot, and the alternative was threading a second
    /// lookup through every call site that already has a `&Registry`.
    pub fn context_length(&self, model: &str) -> Option<u64> {
        self.context_length.get(model).copied()
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
            pools: pools.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
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
    /// Used by virtual-model target selection (`crate::routing`) to decide
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
