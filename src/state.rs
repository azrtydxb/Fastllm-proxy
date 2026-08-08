//! Shared process state.

use anyhow::Context;
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::registry::{Interner, Registry};
use crate::router::Router;
use crate::snapshot::Snapshot;
use crate::source::file::FileSource;
use crate::source::{SnapshotSource, WithLegacyMasterKey};

/// Speaks both schemes: cluster-local vLLM nodes are plain HTTP, but a config
/// may equally point at a TLS-terminated or hosted endpoint.
///
/// `Arc`-wrapped so the one pooled client this crate ever builds (see
/// `upstream::Upstream`'s doc comment) can also be handed to an `HttpSource`
/// without standing up a second pool.
pub type HttpClient = Arc<crate::upstream::Upstream>;

pub struct AppState {
    /// Swapped wholesale on reload; readers never block.
    pub registry: ArcSwap<Registry>,
    pub router: Router,
    pub client: HttpClient,
    pub interner: Interner,
    /// `None` outside `File` mode: `--role all`/`control` and `Http`-mode
    /// `--role proxy` do not treat `--config` as a source of truth for
    /// models or keys, only (optionally) for tuning. `reload()` below is the
    /// only thing that reads this, and only `File` mode ever calls it.
    pub config_path: Option<PathBuf>,

    /// Deprecated `--master-key`/`general_settings.master_key`, carried here
    /// so `reload()` (SIGHUP) can re-merge it into every fetch — see
    /// `source::WithLegacyMasterKey` for why that merge cannot just happen
    /// once at startup.
    pub legacy_master_key: Option<String>,

    /// `Arc`-wrapped so `--role all` can hand `self` to the control plane's
    /// admin API as a `control::api::SnapshotSink` — see `apply_snapshot`
    /// below for why that is the only way in.
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub max_body_bytes: usize,
    pub max_retries: usize,
    /// Bounds time-to-first-byte only. Generation itself is unbounded — a long
    /// completion is not a hung upstream.
    pub upstream_headers_timeout: Duration,
    pub unhealthy_after: u32,

    pub started: Instant,
    pub requests_ok: AtomicU64,
    pub requests_failed: AtomicU64,

    /// The data-plane half of the Snapshot protocol's reverse channel
    /// (`POST /usage`, see `crate::usage`). Nothing calls `usage.record(...)`
    /// yet — see that module's doc comment — but the handle lives here
    /// unconditionally so `proxy::metrics_response` can always report
    /// `usage.dropped()`, in every mode including `File`, where it is a
    /// `UsageReporter::disabled()` that would drop anything recorded on it.
    pub usage: crate::usage::UsageReporter,

    /// P2 rate limiting: per-principal token buckets, one instance for the
    /// life of the process. Deliberately not swapped alongside `snapshot` —
    /// see `crate::limiter::Limiter`'s doc comment for why a bucket must
    /// survive a snapshot reload instead of resetting to full every time an
    /// unrelated admin write triggers one. `Arc`-wrapped so `crate::reconcile`'s
    /// background task (spawned in `Http`-mode `--role proxy` only, see
    /// `main.rs`) can hold its own handle without holding all of `AppState`.
    pub limiter: Arc<crate::limiter::Limiter>,
    /// Counters and histograms the request path writes.
    ///
    /// On `AppState` rather than on the registry because the registry's pools
    /// are rebuilt on every snapshot apply — about once a second — and a
    /// counter that resets that often reads to Prometheus as a restart. See
    /// `telemetry::metrics`.
    pub telemetry: crate::telemetry::Telemetry,

    /// Prompt classifier, rebuilt from the snapshot on every `apply_snapshot`
    /// so the centroids a request is scored against are always the ones the
    /// control plane most recently published.
    ///
    /// `ArcSwap` for the same reason `registry` is: the request path reads it
    /// without a lock, and a reload replaces it wholesale.
    #[cfg(feature = "classifier")]
    pub classifier: ArcSwap<crate::classifier::Classifier>,

    /// The two embedding models, loaded at most once each.
    ///
    /// Tier 2 is a `OnceLock` rather than eagerly loaded because that is the
    /// whole gate: a deployment whose rules never name a refined class never
    /// initialises it, never maps 130MB of weights, and never pays its ~85ms
    /// startup. See `crate::classifier`'s module doc comment.
    #[cfg(feature = "classifier")]
    pub tier1: Option<Arc<crate::classifier::tier1::Tier1>>,
    #[cfg(feature = "classifier-tier2")]
    pub tier2_path: Option<String>,
    #[cfg(feature = "classifier-tier2")]
    pub tier2: std::sync::OnceLock<Option<Arc<crate::classifier::tier2::Tier2>>>,
}

impl AppState {
    /// The *only* way `self.snapshot` may change. Stores the new snapshot and
    /// rebuilds the routing `Registry` from it in the same call, so the two
    /// can never observably disagree — no caller can update one without the
    /// other, because there is no other entry point that writes `snapshot`.
    ///
    /// Every writer goes through this: `source::spawn_poller` (`File`/`Http`
    /// modes), `reload()` below (SIGHUP, `File` mode), and, for `--role all`,
    /// the admin API's `refresh()` — by way of the `control::api::SnapshotSink`
    /// impl on `AppState` a few lines down, which just calls this. A previous
    /// version of this comment claimed that guarantee while `refresh()` still
    /// called `self.snapshot.store(...)` directly and skipped the rebuild
    /// entirely; that bug — a model added through the admin API updated
    /// authorisation but never reached routing, so requests for it 404'd
    /// until a restart — is what this single-entry-point design exists to
    /// make structurally impossible to reintroduce.
    pub fn apply_snapshot(&self, snap: Snapshot) -> anyhow::Result<usize> {
        #[cfg(feature = "classifier")]
        self.rebuild_classifier(&snap);
        self.telemetry
            .rebuild_models(snap.models.iter().map(|m| m.name.as_str()));
        self.snapshot.store(Arc::new(snap));
        self.rebuild_registry_from_snapshot()
    }

    /// An `AppState` wired up for tests, in this module and others.
    ///
    /// No network call is made through it; only `Upstream::new`'s bookkeeping
    /// is exercised (same pattern as `source::http`'s tests).
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        use crate::router::Policy;
        use crate::upstream::{Config as UpstreamConfig, Upstream};

        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let client = Arc::new(Upstream::new(
            UpstreamConfig {
                max_idle_per_host: 1,
                idle_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_secs(1),
            },
            tls,
        ));
        Self {
            #[cfg(feature = "classifier")]
            classifier: ArcSwap::from_pointee(crate::classifier::Classifier::default()),
            #[cfg(feature = "classifier")]
            tier1: None,
            #[cfg(feature = "classifier-tier2")]
            tier2_path: None,
            #[cfg(feature = "classifier-tier2")]
            tier2: std::sync::OnceLock::new(),
            registry: ArcSwap::from_pointee(Registry::default()),
            router: Router::new(Policy::CacheAffinity, 64, 64, 0, 0.0),
            client,
            interner: Interner::default(),
            config_path: None,
            legacy_master_key: None,
            snapshot: Arc::new(ArcSwap::from_pointee(Snapshot::default())),
            max_body_bytes: 1024,
            max_retries: 0,
            upstream_headers_timeout: Duration::from_secs(1),
            unhealthy_after: 1,
            started: Instant::now(),
            requests_ok: AtomicU64::new(0),
            requests_failed: AtomicU64::new(0),
            usage: crate::usage::UsageReporter::disabled(),
            limiter: Arc::new(crate::limiter::Limiter::new()),
            telemetry: crate::telemetry::Telemetry::new(),
        }
    }

    /// Build the views derived from the snapshot this state was *constructed*
    /// with.
    ///
    /// `AppState` is built with a snapshot already in hand, which bypasses
    /// `apply_snapshot` — the single write path — and so bypasses everything
    /// that path derives. The registry is fine because it is built separately
    /// by the same constructor; the classifier was not, so a `--role proxy`
    /// serving an unchanged snapshot classified nothing at all until the
    /// control plane happened to publish a change. Tests never caught it
    /// because `--role all` writes through the admin API, which triggers a
    /// rebuild immediately.
    ///
    /// Call once, immediately after construction.
    pub fn prime_derived_views(&self) {
        let snap = self.snapshot.load();
        #[cfg(feature = "classifier")]
        self.rebuild_classifier(&snap);
        self.telemetry
            .rebuild_models(snap.models.iter().map(|m| m.name.as_str()));
    }

    /// Rebuild the classifier from a snapshot's published centroids.
    ///
    /// Called from `apply_snapshot` alongside the registry rebuild, for the
    /// same reason: two views derived from one snapshot must not be able to
    /// diverge, so they are refreshed in the same call rather than by whoever
    /// remembers to.
    #[cfg(feature = "classifier")]
    fn rebuild_classifier(&self, snap: &Snapshot) {
        use crate::classifier::{Classifier, PromptClass, Tier};
        let classes = snap
            .prompt_classes
            .iter()
            .filter_map(|c| {
                // A class the control plane could not embed, or one naming a
                // tier this build does not understand, is dropped rather than
                // defaulted — the same rule the snapshot applies to a backend
                // whose protocol it cannot parse.
                if c.centroid.is_empty() {
                    return None;
                }
                Some(PromptClass {
                    name: c.name.clone(),
                    tier: Tier::parse(&c.tier)?,
                    centroid: Some(c.centroid.clone()),
                    min_margin: c.min_margin,
                    refines: c.refines.clone(),
                })
            })
            .collect::<Vec<_>>();
        self.classifier.store(Arc::new(Classifier::new(classes)));
    }

    /// Classify a request body, or `None` when there is nothing to classify
    /// against.
    ///
    /// The early return is the fast path's guarantee: with no classes
    /// configured this costs one atomic load and a length check, so a
    /// deployment that does not use semantic routing pays nothing for its
    /// existence.
    #[cfg(feature = "classifier")]
    pub fn classify(&self, body: &[u8]) -> Option<crate::classifier::Classification> {
        let classifier = self.classifier.load();
        if classifier.is_empty() {
            return None;
        }
        let tier1 = self.tier1.as_ref()?;
        let text = std::str::from_utf8(body).ok()?;
        let embedding = tier1.embed(text);
        classifier.classify(&embedding, || self.refined_embedding(text))
    }

    #[cfg(not(feature = "classifier"))]
    pub fn classify(&self, _body: &[u8]) -> Option<std::convert::Infallible> {
        None
    }

    /// Embed with the transformer, loading it on first use.
    ///
    /// Only ever called when an active refined class named the tier-1 answer,
    /// so the load itself is gated on configuration rather than on a flag.
    #[cfg(feature = "classifier-tier2")]
    fn refined_embedding(&self, text: &str) -> Option<Vec<f32>> {
        let loaded = self.tier2.get_or_init(|| {
            let path = self.tier2_path.as_ref()?;
            match crate::classifier::tier2::Tier2::load(path) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    // Logged once, not per request: `OnceLock` means a failed
                    // load stays failed, and every later request quietly falls
                    // back to the fast tier's answer.
                    tracing::error!(error = %e, path = %path, "tier-2 classifier failed to load; \
                        refined classes will fall back to the fast tier");
                    None
                }
            }
        });
        loaded.as_ref()?.embed(text)
    }

    #[cfg(all(feature = "classifier", not(feature = "classifier-tier2")))]
    fn refined_embedding(&self, _text: &str) -> Option<Vec<f32>> {
        None
    }

    /// Rebuild the routing `Registry` from the snapshot already stored in
    /// `self.snapshot`, and swap it in.
    ///
    /// Live backends carry over, so a reload does not reset health or lose
    /// in-flight accounting for connections still streaming. Not `pub` to
    /// external callers as a way to update policy — `apply_snapshot` is,
    /// precisely so a snapshot write can never skip this step.
    fn rebuild_registry_from_snapshot(&self) -> anyhow::Result<usize> {
        let snap = self.snapshot.load();
        let current = self.registry.load();
        let next = Registry::build_from_snapshot(&snap, &self.interner, Some(&current))?;
        let count = next.backends().len();
        self.registry.store(Arc::new(next));
        Ok(count)
    }

    /// SIGHUP's handler in `File` mode: re-read the config file right now
    /// rather than waiting for the next poll tick, and apply it via
    /// `apply_snapshot` so registry and snapshot move together.
    ///
    /// Only meaningful when `config_path` is `Some` and is the actual source
    /// of truth, i.e. `File` mode — `main.rs` only wires the SIGHUP listener
    /// up in that case. `Http` and `all` deployments are kept current by
    /// `source::spawn_poller` and by the admin API's write-time refresh
    /// respectively, and re-parsing `config_path` there would be reloading
    /// the wrong source of truth (or, in `all`'s case, a path that may not
    /// even be set).
    pub async fn reload(&self) -> anyhow::Result<usize> {
        let path = self
            .config_path
            .clone()
            .context("reload requires --config (File mode only)")?;
        let source =
            WithLegacyMasterKey::new(FileSource::new(path), self.legacy_master_key.clone());
        let snap = source
            .fetch(None)
            .await?
            .context("config produced no snapshot on reload")?;
        self.apply_snapshot(snap)
    }
}

/// Lets `--role all` hand `Arc<AppState>` directly to `control::api::serve` as
/// its snapshot sink, so the admin API's writes and the proxy's routing table
/// share one update path (`AppState::apply_snapshot`) instead of two that
/// could drift apart — the exact bug this type's existence fixes.
#[cfg(feature = "control")]
impl crate::control::api::SnapshotSink for AppState {
    fn store_snapshot(&self, snap: Snapshot) -> Arc<Snapshot> {
        if let Err(e) = self.apply_snapshot(snap) {
            tracing::error!(
                error = %e,
                "snapshot applied but registry rebuild failed; keeping previous routing table"
            );
        }
        self.snapshot.load_full()
    }

    fn current_snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{BackendDef, ModelDef};

    fn test_state() -> AppState {
        AppState::for_test()
    }

    fn snapshot_with_model(name: &str) -> Snapshot {
        Snapshot {
            version: 1,
            models: vec![ModelDef {
                name: name.to_string(),
                backends: vec![BackendDef {
                    api_base: "http://10.0.0.1:8000".into(),
                    upstream_model: name.to_string(),
                    api_key: None,
                    ..Default::default()
                }],
            }],
            ..Snapshot::default()
        }
    }

    /// The regression this task's Critical review finding described: a
    /// snapshot write must never be observable without the registry it
    /// implies. `apply_snapshot` is the only method that may write
    /// `self.snapshot` (see its doc comment), so this is really a test of
    /// that invariant, not just of one call site.
    #[test]
    fn apply_snapshot_makes_a_new_model_routable() {
        let state = test_state();
        assert!(state.registry.load().pool("new-model").is_none());

        state
            .apply_snapshot(snapshot_with_model("new-model"))
            .unwrap();

        assert!(
            state.registry.load().pool("new-model").is_some(),
            "the registry must reflect a model added by a snapshot write"
        );
    }

    /// Exercises the exact path `--role all`'s admin API takes on every
    /// key/model write (`control::api::refresh` calls
    /// `ctx.cache.store_snapshot(...)`, and in `all` mode `ctx.cache` is this
    /// `AppState`). This is the test that would have caught the Critical
    /// finding: `refresh()` used to call `self.snapshot.store(...)` directly
    /// and skip the registry rebuild entirely, so a model added through the
    /// admin API updated authorisation but never became routable until a
    /// restart.
    #[cfg(feature = "control")]
    #[test]
    fn the_admin_apis_write_path_reaches_the_registry() {
        use crate::control::api::SnapshotSink;

        let state = test_state();
        assert!(state.registry.load().pool("added-via-admin-api").is_none());

        SnapshotSink::store_snapshot(&state, snapshot_with_model("added-via-admin-api"));

        assert!(
            state.registry.load().pool("added-via-admin-api").is_some(),
            "a snapshot written through the admin API's sink must reach routing"
        );
    }
}
