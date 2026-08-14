//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use fastllm_proxy::config::FileConfig;
use fastllm_proxy::registry::{Interner, Registry};
use fastllm_proxy::router::{Policy, Router};
use fastllm_proxy::snapshot::Snapshot;
use fastllm_proxy::source::file::FileSource;
use fastllm_proxy::source::http::HttpSource;
use fastllm_proxy::source::{spawn_poller, SnapshotSource};
use fastllm_proxy::state::AppState;
use fastllm_proxy::{health, proxy, upstream};

#[derive(Parser, Debug)]
#[command(
    name = "fastllm-proxy",
    version,
    about = "Low-latency OpenAI-compatible inference gateway"
)]
struct Cli {
    /// One-shot maintenance commands (`import`). Absent means: start the
    /// server per `--role`, the historical default behaviour.
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the model config. LiteLLM-format configs are accepted as-is.
    ///
    /// Required in `File` mode (`--role proxy` with no `--control-url`),
    /// where it is the only source of models and keys. Optional everywhere
    /// else — `--role all`/`control` and `Http`-mode `--role proxy` get
    /// models and keys from a database or control plane, and use this file
    /// (if given) only for the `fastllm:`/`general_settings.master_key`
    /// tuning knobs below.
    #[arg(short, long, env = "FASTLLM_CONFIG")]
    config: Option<PathBuf>,

    /// Address to bind. Defaults to loopback; set 0.0.0.0 deliberately.
    #[arg(long, default_value = "127.0.0.1", env = "FASTLLM_HOST")]
    host: String,

    #[arg(short, long, default_value_t = 4000, env = "FASTLLM_PORT")]
    port: u16,

    /// Require this bearer token from clients. Overrides `general_settings.master_key`.
    #[arg(long, env = "FASTLLM_MASTER_KEY")]
    master_key: Option<String>,

    #[arg(long, value_enum, default_value_t = Policy::CacheAffinity)]
    policy: Policy,

    /// POST a JSON notification here when a backend goes down or recovers,
    /// or when a snapshot rebuild fails after a write.
    ///
    /// `--role all`/`control` only: these are things the control plane
    /// learns, from health reports and its own rebuilds.
    #[arg(long, env = "FASTLLM_WEBHOOK_URL")]
    webhook_url: Option<String>,

    /// Sign each webhook body with HMAC-SHA256 in `x-fastllm-signature`.
    ///
    /// A webhook endpoint is reachable by anyone who learns its address, so
    /// a receiver that acts on notifications wants to know they came from
    /// here.
    #[arg(long, env = "FASTLLM_WEBHOOK_SECRET")]
    webhook_secret: Option<String>,

    /// Seconds between health sweeps.
    #[arg(long, default_value_t = 10)]
    health_interval: u64,

    /// Seconds a health probe may take before it counts as a failure.
    #[arg(long, default_value_t = 3)]
    health_timeout: u64,

    /// Seconds to wait for upstream response headers. Does not bound generation.
    #[arg(long, default_value_t = 120)]
    upstream_timeout: u64,

    /// Alternate backends to try when one fails before any bytes are sent.
    #[arg(long, default_value_t = 2)]
    max_retries: usize,

    /// Largest request body accepted, in MiB.
    #[arg(long, default_value_t = 64)]
    max_body_mb: usize,

    /// Idle upstream connections kept per backend.
    #[arg(long, default_value_t = 256)]
    pool_max_idle: usize,

    /// Worker threads. Defaults to the core count.
    #[arg(long)]
    workers: Option<usize>,

    /// Seconds between snapshot refreshes: a `File`-mode check for an edited
    /// config file, or an `Http`-mode poll of the control plane. 0 disables
    /// the watch; in `File` mode SIGHUP is then the only way to reload.
    #[arg(long, default_value_t = 5, env = "FASTLLM_CONFIG_POLL")]
    config_poll: u64,

    #[arg(long, default_value = "info", env = "FASTLLM_LOG")]
    log: String,

    /// Ceilings on the response cache. Both matter: a thousand embedding
    /// responses is nothing and a thousand completions is hundreds of
    /// megabytes, so either alone leaves the other unbounded. Only reached by
    /// models that turn caching on.
    #[arg(long, default_value_t = 4096, env = "FASTLLM_CACHE_MAX_ENTRIES")]
    cache_max_entries: usize,

    #[arg(long, default_value_t = 64 * 1024 * 1024, env = "FASTLLM_CACHE_MAX_BYTES")]
    cache_max_bytes: usize,

    /// How long to let in-flight requests finish after SIGTERM before the
    /// process exits. Kubernetes SIGKILLs at `terminationGracePeriodSeconds`
    /// (30 by default), so this sits under it; 0 exits immediately.
    #[arg(long, default_value_t = 25, env = "FASTLLM_SHUTDOWN_GRACE")]
    shutdown_grace: u64,

    /// Seconds between health reports to the control plane. Backend health
    /// only exists in the data plane, so this is the only way a management UI
    /// can see it.
    #[arg(long, default_value_t = 10, env = "FASTLLM_HEALTH_REPORT_INTERVAL")]
    health_report_interval: u64,

    /// `text` for humans, `json` for a log collector.
    #[arg(long, value_enum, default_value_t = LogFormat::Text, env = "FASTLLM_LOG_FORMAT")]
    log_format: LogFormat,

    /// OTLP/gRPC collector for traces, e.g. `http://collector:4317`. Unset
    /// disables tracing entirely, which is the default.
    #[cfg(feature = "otel")]
    #[arg(long, env = "FASTLLM_OTEL_ENDPOINT")]
    otel_endpoint: Option<String>,

    /// Trace one request in this many. 1 traces everything, which is only
    /// sensible at low volume or while debugging.
    #[cfg(feature = "otel")]
    #[arg(long, default_value_t = 100, env = "FASTLLM_OTEL_SAMPLE_ONE_IN")]
    otel_sample_one_in: u64,

    #[cfg(feature = "otel")]
    #[arg(
        long,
        default_value = "fastllm-proxy",
        env = "FASTLLM_OTEL_SERVICE_NAME"
    )]
    otel_service_name: String,

    /// Directory or HuggingFace repo id for the fast-tier classifier model.
    ///
    /// A directory is what a container should use: the Dockerfile bakes the
    /// model in so startup does no network I/O. Unset means semantic routing
    /// is unavailable — prompt classes are still stored and listed, they just
    /// cannot match, and every rule naming one falls through.
    #[arg(long, env = "FASTLLM_CLASSIFIER_MODEL")]
    classifier_model: Option<String>,

    /// Directory holding the refined-tier ONNX model and tokeniser.
    ///
    /// Loaded lazily, and only if some routing rule names a class that refines
    /// a fast-tier one — see `crate::classifier`. Unset means refined classes
    /// silently fall back to the fast tier's answer.
    #[arg(long, env = "FASTLLM_CLASSIFIER_TIER2_MODEL")]
    classifier_tier2_model: Option<String>,

    /// Forwarding only (default), the control plane and forwarding in one
    /// process, or the admin API and database only.
    #[arg(long, value_enum, default_value_t = Role::Proxy, env = "FASTLLM_ROLE")]
    role: Role,

    /// Required for `--role all` and `--role control`; unused by `--role proxy`.
    #[arg(long, env = "FASTLLM_DATABASE_URL")]
    database_url: Option<String>,

    /// Control plane to poll in `--role proxy` mode. Omitted means `File`
    /// mode: no control plane at all, policy comes from `--config` alone.
    #[arg(long, env = "FASTLLM_CONTROL_URL")]
    control_url: Option<String>,

    /// Bearer token this process presents to a control plane (`--role
    /// proxy`), or requires of callers of its own admin API (`--role
    /// all`/`control`).
    #[arg(long, env = "FASTLLM_PROXY_TOKEN")]
    proxy_token: Option<String>,

    /// Where `Http` mode keeps its last-known-good snapshot, so a control
    /// plane outage degrades to "stop learning about changes" rather than
    /// "stop serving".
    #[arg(long, default_value = "/var/lib/fastllm/snapshot.json")]
    snapshot_cache: PathBuf,

    /// Bind port for the admin API (`--role all`/`control`).
    #[arg(long, default_value_t = 4001)]
    admin_port: u16,

    /// Seconds between the control plane rebuilding its snapshot from the
    /// database on its own initiative, independent of admin API writes.
    ///
    /// Without this, a running `--role all`/`control` process only ever
    /// rebuilds at startup and right after its own `POST`/`DELETE
    /// /admin/keys` — so `fastllm-proxy import` (a separate process) and a
    /// hand-written `UPDATE`/`INSERT` against Postgres (`deploy/README.md`'s
    /// documented fix for a moved backend) never reach it at all. 0 disables
    /// the rebuild, same convention as `--config-poll`.
    #[arg(long, default_value_t = 5, env = "FASTLLM_SNAPSHOT_REBUILD_INTERVAL")]
    snapshot_rebuild_interval: u64,

    /// PEM certificate (chain) for the admin API's `/snapshot` and `/usage`
    /// listener (`--role all`/`control`). Requires `--tls-key`. Absent means
    /// plain HTTP, which is legitimate for a dev deployment with no real
    /// backend credentials but not otherwise — see the design doc's Snapshot
    /// protocol section: `/snapshot` carries usable upstream credentials.
    #[arg(long, env = "FASTLLM_TLS_CERT")]
    tls_cert: Option<PathBuf>,

    /// PEM private key matching `--tls-cert`.
    #[arg(long, env = "FASTLLM_TLS_KEY")]
    tls_key: Option<PathBuf>,

    /// Seconds between this replica reporting locally observed rate-limit
    /// counts to the control plane and applying the allowance it hands
    /// back (P2 reconciliation, `crate::reconcile`). Only meaningful in
    /// `Http`-mode `--role proxy` (`--control-url` set) — `File` mode has no
    /// control plane to reconcile with, and `--role all` needs no
    /// reconciliation at all (see `crate::limiter`'s doc comment). 0
    /// disables it, same convention as `--config-poll` and
    /// `--snapshot-rebuild-interval`.
    #[arg(
        long,
        default_value_t = 5,
        env = "FASTLLM_RATE_LIMIT_RECONCILE_INTERVAL"
    )]
    rate_limit_reconcile_interval: u64,

    /// Extra CA certificate(s) (PEM, one or more concatenated) trusted in
    /// addition to the system root store, for a `https://` control plane or
    /// backend whose certificate was not issued by a public CA — the normal
    /// case for an in-cluster cert-manager-issued certificate. Without this,
    /// a self-signed or privately-issued control-plane cert cannot be
    /// verified and `Http`-mode `--control-url https://...` fails the TLS
    /// handshake.
    #[arg(long, env = "FASTLLM_CA_BUNDLE")]
    ca_bundle: Option<PathBuf>,
}

/// One-shot maintenance commands, as opposed to `Cli`'s server-starting flags.
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Seed `models`/`model_backends` from a LiteLLM-format config file.
    ///
    /// The migration path off a file-driven deployment onto the control
    /// plane: run this once per environment (safely more than once — it is
    /// idempotent) instead of hand-writing the equivalent `INSERT`s.
    Import {
        /// LiteLLM-format config to import, e.g. an exported ConfigMap.
        #[arg(long)]
        config: PathBuf,

        /// Database to seed. Distinct from `--database-url` on `Cli` so
        /// `import` never has to be issued alongside `--role`.
        #[arg(long, env = "FASTLLM_DATABASE_URL")]
        database_url: String,
    },

    /// One-shot migration: re-encrypt any `model_backends.upstream_api_key`
    /// rows still holding pre-encryption plaintext (see
    /// `migrations/0004_encrypted_upstream_api_key.sql`). Safe to run more
    /// than once — an already-migrated row is left alone.
    ReencryptBackends {
        /// Database to migrate. Distinct from `--database-url` on `Cli` for
        /// the same reason `Import::database_url` is: this never has to be
        /// issued alongside `--role`.
        #[arg(long, env = "FASTLLM_DATABASE_URL")]
        database_url: String,
    },

    /// Bootstrap (or reset) an admin login for the management UI (P4).
    ///
    /// The one gap `PUT /admin/principals/{id}/password` cannot close on its
    /// own: that route is (correctly) gated behind a session cookie, and a
    /// freshly migrated database has no session anyone can ever obtain — its
    /// `principals` rows have no `password_hash` set. This is how the very
    /// first one gets one, run once by whoever holds cluster access, the
    /// same trust boundary `import`'s `--database-url` already relies on.
    /// Safe to run again later against an existing user to reset a
    /// forgotten password.
    SetPassword {
        /// The login name. Created as a new `kind = 'user'` principal if no
        /// principal by this name exists yet, and granted the `admin` role
        /// unless it already holds one that grants `config:write` — see
        /// `control::auth::bootstrap_admin_user`. Checking for the permission
        /// rather than for "any role at all" is what stops the seeded
        /// `bootstrap` principal, which already holds `inference`, from
        /// becoming an account that can log in but administer nothing.
        #[arg(long)]
        name: String,

        /// The new password, in plaintext, on the command line or in
        /// `FASTLLM_BOOTSTRAP_PASSWORD` — an operator's terminal history or
        /// a one-shot Job's env is the trust boundary here, the same as
        /// `--proxy-token`/`FASTLLM_PROXY_TOKEN` elsewhere in this CLI.
        #[arg(long, env = "FASTLLM_BOOTSTRAP_PASSWORD")]
        password: String,

        /// Database to write to. Distinct from `--database-url` on `Cli` for
        /// the same reason `Import::database_url` is.
        #[arg(long, env = "FASTLLM_DATABASE_URL")]
        database_url: String,
    },

    /// Fill in model prices from a published catalogue, so nobody types them.
    ///
    /// Only touches models whose price is unset, unless `--overwrite`: an
    /// operator who entered a negotiated rate should not have it replaced by a
    /// list price on the next run.
    ///
    /// This is the *fallback* source. Where a provider reports what it charged
    /// — OpenRouter returns `usage.cost` unasked — that figure is used instead
    /// and nothing here competes with it.
    #[cfg(feature = "control")]
    SyncPrices {
        #[arg(long, env = "FASTLLM_DATABASE_URL")]
        database_url: String,

        #[arg(long, value_enum, default_value_t = fastllm_proxy::control::pricing::Source::Both)]
        source: fastllm_proxy::control::pricing::Source,

        /// Replace prices that are already set, not only fill in the missing.
        #[arg(long)]
        overwrite: bool,

        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Measure the classifier where it actually runs.
    ///
    /// The tier-2 cost in `docs/classifier.md` was measured on a laptop, and
    /// the deployed container turned out to be more than an order of magnitude
    /// slower. Guessing at why — thread counts, core counts, token windows —
    /// is what this exists to stop: it ships inside the image, so
    /// `kubectl exec` measures the pod's real CPU quota rather than a
    /// developer's machine.
    #[cfg(feature = "classifier-tier2")]
    ClassifyBench {
        /// Fast-tier model directory. Defaults to the image's baked-in path.
        #[arg(long, env = "FASTLLM_CLASSIFIER_MODEL")]
        classifier_model: Option<String>,

        /// Refined-tier model directory.
        #[arg(long, env = "FASTLLM_CLASSIFIER_TIER2_MODEL")]
        classifier_tier2_model: Option<String>,

        #[arg(long, default_value_t = 20)]
        iterations: u32,

        /// Concurrency to measure, so the effect of `Tier2`'s session mutex is
        /// visible rather than inferred.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// Control plane and forwarding in one process. Opt-in, not the default:
    /// it requires `--database-url`, and every deployment that predates the
    /// control plane runs with no `--role` flag at all. If `all` were the
    /// default, pulling this binary into an existing deployment (same image
    /// tag, same flags, no `--database-url` anywhere in the pod spec) would
    /// make every new pod exit at startup — a CrashLoopBackOff with no config
    /// change to point at, and nothing for `maxUnavailable: 0` to roll back
    /// to once the last old pod is gone.
    All,
    /// Database, admin API and `/snapshot` only — no proxy listener.
    Control,
    /// Forwarding only, against a control plane or a config file. The
    /// default, precisely because it is what today's flags already mean:
    /// `--config` alone still boots `File` mode exactly as it always has, no
    /// database required, no behaviour change on upgrade. `all` and
    /// `control` are both explicit opt-ins via `--role`.
    Proxy,
}

/// Human-readable lines, or one JSON object per line.
///
/// The default stays human-readable because that is what somebody running this
/// locally or reading `kubectl logs` wants. `--log-format json` is for the
/// deployment: a log line is only useful to a collector if its fields survive
/// as fields, and a regex over "backend=http://... status=502" is the thing
/// every log pipeline eventually regrets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

fn init_logging(cli: &Cli) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::Layer as _;

    let filter = tracing_subscriber::EnvFilter::try_new(&cli.log)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    // Boxed so both formats have one type and the OTLP layer below can be
    // composed once rather than per branch.
    let fmt = match cli.log_format {
        LogFormat::Text => tracing_subscriber::fmt::layer().with_target(false).boxed(),
        // `flatten_event` puts the message and its fields at the top level
        // rather than nested under "fields", which is what every collector
        // expects to index without a transform step.
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .boxed(),
    };

    // One registry carrying the log layer and, when configured, the OTLP layer
    // beside it. Two separate `init` calls would mean the second silently
    // losing to the first, and the symptom — logs but no traces, or the
    // reverse — points nowhere near the cause.
    #[cfg(feature = "otel")]
    {
        let otel = cli.otel_endpoint.as_ref().and_then(|endpoint| {
            let cfg = fastllm_proxy::telemetry::tracing_otel::Config {
                endpoint: endpoint.clone(),
                sample_one_in: cli.otel_sample_one_in,
                service_name: cli.otel_service_name.clone(),
            };
            match fastllm_proxy::telemetry::tracing_otel::layer(&cfg) {
                Ok(layer) => Some(layer),
                Err(e) => {
                    // Not fatal. A collector unreachable at startup must not
                    // stop the proxy serving traffic — that would turn an
                    // observability dependency into an availability one.
                    eprintln!("tracing disabled: could not build the OTLP exporter: {e:#}");
                    None
                }
            }
        });
        registry.with(fmt).with(otel).init();
    }
    #[cfg(not(feature = "otel"))]
    registry.with(fmt).init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(n) = cli.workers {
        builder.worker_threads(n.max(1));
    }
    let runtime = builder.build()?;
    // Logging is initialised *inside* the runtime, not before it. The OTLP
    // exporter is a gRPC client, and building one outside a runtime panics in
    // hyper-util's executor — which is a startup crash, not a disabled
    // exporter, so it has to be here rather than a line earlier.
    runtime.block_on(async {
        init_logging(&cli);
        run(cli).await
    })
}

async fn run(cli: Cli) -> Result<()> {
    if let Some(command) = &cli.command {
        return run_command(command).await;
    }
    match cli.role {
        Role::Control => run_control(cli).await,
        Role::Proxy => run_data_plane(cli).await,
        Role::All => run_all(cli).await,
    }
}

/// Runs a one-shot command and returns; never starts a server or a listener.
async fn run_command(command: &Command) -> Result<()> {
    match command {
        Command::Import {
            config,
            database_url,
        } => run_import(config, database_url).await,
        Command::ReencryptBackends { database_url } => run_reencrypt_backends(database_url).await,
        Command::SetPassword {
            name,
            password,
            database_url,
        } => run_set_password(name, password, database_url).await,
        #[cfg(feature = "control")]
        Command::SyncPrices {
            database_url,
            source,
            overwrite,
            dry_run,
        } => run_sync_prices(database_url, *source, *overwrite, *dry_run).await,
        #[cfg(feature = "classifier-tier2")]
        Command::ClassifyBench {
            classifier_model,
            classifier_tier2_model,
            iterations,
            concurrency,
        } => run_classify_bench(
            classifier_model.as_deref(),
            classifier_tier2_model.as_deref(),
            *iterations,
            *concurrency,
        ),
    }
}

/// The flags this process was started with, for `GET /admin/config`.
///
/// Assembled here because this is the only place that has the parsed CLI; the
/// control plane would otherwise have to guess, and a settings screen showing
/// a guessed default is worse than one showing nothing.
#[cfg(feature = "control")]
/// The webhook sender for this process, or a disabled one when no URL was
/// given — which is the default, and costs nothing.
fn webhook_sender(
    cli: &Cli,
    client: &Arc<fastllm_proxy::upstream::Upstream>,
) -> Arc<fastllm_proxy::webhook::WebhookSender> {
    Arc::new(match &cli.webhook_url {
        Some(url) => fastllm_proxy::webhook::spawn(
            url.clone(),
            cli.webhook_secret.clone(),
            Arc::clone(client),
            // Small on purpose: delivering a burst of stale alerts once a
            // receiver returns is worse than dropping them. See the module
            // doc comment.
            64,
        ),
        None => fastllm_proxy::webhook::WebhookSender::disabled(),
    })
}

fn deployment_facts(cli: &Cli, role: &str) -> fastllm_proxy::control::api::Deployment {
    fastllm_proxy::control::api::Deployment {
        role: role.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config_poll_seconds: cli.config_poll,
        health_report_interval_seconds: cli.health_report_interval,
        cache_max_entries: cli.cache_max_entries,
        cache_max_bytes: cli.cache_max_bytes,
        // The flags themselves only exist in an `otel` build; reporting
        // "no endpoint" from a build that could not have one is the honest
        // answer rather than an absent field the UI has to guess about.
        #[cfg(feature = "otel")]
        otel_endpoint: cli.otel_endpoint.clone(),
        #[cfg(not(feature = "otel"))]
        otel_endpoint: None,
        #[cfg(feature = "otel")]
        otel_sample_one_in: cli.otel_sample_one_in,
        #[cfg(not(feature = "otel"))]
        otel_sample_one_in: 0,
        classifier_tier1: cfg!(feature = "classifier"),
        classifier_tier2: cfg!(feature = "classifier-tier2"),
        policy: format!("{:?}", cli.policy)
            .chars()
            .flat_map(|c| {
                if c.is_uppercase() {
                    vec!['-', c.to_ascii_lowercase()]
                } else {
                    vec![c]
                }
            })
            .skip(1)
            .collect(),
        webhook_configured: cli.webhook_url.is_some(),
        webhook_signed: cli.webhook_secret.is_some(),
    }
}

/// `fastllm-proxy sync-prices`: fill in model prices from a published
/// catalogue.
#[cfg(feature = "control")]
async fn run_sync_prices(
    database_url: &str,
    source: fastllm_proxy::control::pricing::Source,
    overwrite: bool,
    dry_run: bool,
) -> Result<()> {
    let client = Arc::new(upstream::Upstream::new(
        upstream::Config {
            max_idle_per_host: 2,
            idle_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
        },
        tls_config(None)?,
    ));
    let pool = fastllm_proxy::control::db::connect(database_url).await?;

    // The work itself lives in the library, shared with `POST
    // /admin/prices/sync`. Two copies of "which source wins" would drift, and
    // that rule is the interesting part.
    let report =
        fastllm_proxy::control::pricing::sync(&pool, &client, source, overwrite, dry_run).await?;

    for (name, price) in &report.changes {
        println!(
            "  {name}: in {} out {} per Mtok",
            price.input_per_mtok, price.output_per_mtok
        );
    }
    println!(
        "{} {} model(s); {} already priced, {} with no match",
        if dry_run { "would update" } else { "updated" },
        report.updated,
        report.skipped,
        report.unmatched
    );
    if !dry_run && report.updated > 0 {
        println!("the next snapshot rebuild picks these up; no restart needed");
    }
    Ok(())
}

/// Measure both classifier tiers under the CPU this process actually has.
#[cfg(feature = "classifier-tier2")]
fn run_classify_bench(
    tier1_dir: Option<&str>,
    tier2_dir: Option<&str>,
    iterations: u32,
    concurrency: usize,
) -> Result<()> {
    use fastllm_proxy::classifier::{tier1::Tier1, tier2};
    use std::sync::Arc;
    use std::time::Instant;

    // The first thing to establish, because every thread-count question below
    // depends on it: what this process believes it may use. In a container that
    // is the cgroup quota if the runtime reads it, and the node's core count if
    // it does not — and the difference is the whole hypothesis.
    println!(
        "available_parallelism: {:?}",
        std::thread::available_parallelism().map(|n| n.get())
    );
    for path in [
        "/sys/fs/cgroup/cpu.max",
        "/sys/fs/cgroup/cpu/cpu.cfs_quota_us",
    ] {
        if let Ok(v) = std::fs::read_to_string(path) {
            println!("{path}: {}", v.trim());
        }
    }

    let prompt = "Why does this Rust code fail the borrow checker, and how do I                   restructure the function so the borrow ends before the move?";

    if let Some(dir) = tier1_dir {
        let t1 = Tier1::load(dir)?;
        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(t1.embed(prompt));
        }
        println!(
            "tier1                       {:>8.0} us/prompt",
            start.elapsed().as_micros() as f64 / f64::from(iterations)
        );
    }

    let Some(dir) = tier2_dir else {
        println!("no tier-2 model path given; skipping the refined tier");
        return Ok(());
    };

    // One row per configuration, so the answer is a comparison rather than a
    // single number needing interpretation.
    for threads in [None, Some(1), Some(2), Some(4), Some(8)] {
        for max_tokens in [128, 256] {
            let options = tier2::Options {
                intra_threads: threads,
                max_tokens,
            };
            let loaded = Instant::now();
            let t2 = match tier2::Tier2::load_with(dir, options) {
                Ok(t) => t,
                Err(e) => {
                    println!("threads={threads:?} tokens={max_tokens}: load failed: {e:#}");
                    continue;
                }
            };
            let load_ms = loaded.elapsed().as_millis();

            let start = Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(t2.embed(prompt));
            }
            let serial_us = start.elapsed().as_micros() as f64 / f64::from(iterations);

            // The same work spread over threads. `Tier2` holds one session
            // behind a mutex, so this is where serialisation shows up as
            // throughput that does not improve.
            let shared = Arc::new(t2);
            let start = Instant::now();
            let handles: Vec<_> = (0..concurrency)
                .map(|_| {
                    let t = Arc::clone(&shared);
                    std::thread::spawn(move || {
                        for _ in 0..iterations {
                            std::hint::black_box(t.embed(prompt));
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().ok();
            }
            let total = iterations as f64 * concurrency as f64;
            let concurrent_us = start.elapsed().as_micros() as f64 / total;

            println!(
                "tier2 threads={:<5} tokens={max_tokens:<4} load={load_ms:>5}ms  \
                 serial={serial_us:>9.0} us  at x{concurrency}={concurrent_us:>9.0} us/prompt",
                threads.map_or("auto".to_string(), |n| n.to_string()),
            );
        }
    }
    Ok(())
}

/// `fastllm-proxy import --config <path> --database-url <url>`: connects,
/// migrates (via `control::db::connect`), imports, and prints a summary for
/// whoever is running the migration by hand.
async fn run_import(config: &std::path::Path, database_url: &str) -> Result<()> {
    #[cfg(feature = "control")]
    {
        // Loaded before any I/O, so a missing/malformed key fails fast
        // rather than after a database round trip. `import` writes
        // `upstream_api_key`, so it needs the key exactly like `--role
        // control`/`all` do — see `EncryptionKey::from_env` for why there is
        // no fallback to storing plaintext.
        let key = fastllm_proxy::control::secrets::EncryptionKey::from_env()?;
        let cfg = FileConfig::load(config)?;
        let pool = fastllm_proxy::control::db::connect(database_url).await?;
        let summary = fastllm_proxy::control::import::import(&pool, &cfg, &key).await?;
        // Both halves of every count, because a re-import legitimately
        // creates nothing and "0 new principal(s)" alone reads as a failure
        // rather than as convergence. Never the key plaintext: the file
        // already holds it, and `import` must not produce a second copy in a
        // terminal buffer or a CI log.
        println!(
            "import complete: {} new model(s), {} new backend(s), \
             {} new principal(s) ({} already present), {} new key(s) ({} updated in place), \
             {} new grant(s) ({} already present, {} revoked)",
            summary.models,
            summary.backends,
            summary.principals,
            summary.principals_existing,
            summary.keys,
            summary.keys_existing,
            summary.grants,
            summary.grants_existing,
            summary.grants_revoked,
        );
        Ok(())
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = (config, database_url);
        anyhow::bail!("import requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// `fastllm-proxy reencrypt-backends --database-url <url>`: the one-shot
/// migration for rows written before encryption-at-rest existed. See
/// `control::import::reencrypt_plaintext_backends` for the detection
/// strategy and why this is a command an operator runs once rather than a
/// format the read path has to tolerate forever.
async fn run_reencrypt_backends(database_url: &str) -> Result<()> {
    #[cfg(feature = "control")]
    {
        let key = fastllm_proxy::control::secrets::EncryptionKey::from_env()?;
        let pool = fastllm_proxy::control::db::connect(database_url).await?;
        let migrated =
            fastllm_proxy::control::import::reencrypt_plaintext_backends(&pool, &key).await?;
        println!("reencrypt-backends complete: {migrated} row(s) migrated to ciphertext");
        Ok(())
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = database_url;
        anyhow::bail!("reencrypt-backends requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// `fastllm-proxy set-password --name <name> --password <password> --database-url <url>`:
/// see `Command::SetPassword`'s doc comment for why this exists at all.
/// Prints nothing but a plain confirmation — never the password, which the
/// caller already has, and never a hash, which is not this command's job to
/// disclose either.
async fn run_set_password(name: &str, password: &str, database_url: &str) -> Result<()> {
    #[cfg(feature = "control")]
    {
        let pool = fastllm_proxy::control::db::connect(database_url).await?;
        let principal_id =
            fastllm_proxy::control::auth::bootstrap_admin_user(&pool, name, password).await?;
        println!(
            "set-password complete: principal {name:?} (id {principal_id}) can now log into \
             the admin UI and holds a role granting config:write"
        );
        Ok(())
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = (name, password, database_url);
        anyhow::bail!("set-password requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// `--config` supplies tuning (`fastllm:`/`general_settings.master_key`) in
/// every role, but is only the source of truth for models and keys in `File`
/// mode. Roles that do not need it that way (`control`, `all`, `Http`-mode
/// `proxy`) treat an absent file as "use defaults", not an error — requiring
/// a meaningless YAML file for a control-plane-only deployment would be its
/// own bug. `File` mode enforces its own stricter requirement at its call
/// site in `run_data_plane`.
fn load_tuning_config(path: Option<&PathBuf>) -> Result<FileConfig> {
    match path {
        Some(p) => FileConfig::load(p),
        None => Ok(FileConfig::default()),
    }
}

/// TLS for the admin API's `/snapshot` + `/usage` listener, from
/// `--tls-cert`/`--tls-key`. `None` means plain HTTP — `control::api::serve`
/// is the one place that turns that into the startup warning the design
/// requires, so a caller here does not have to remember to log anything.
///
/// Requiring both flags together (rather than treating a lone `--tls-key` as
/// "ignore it") turns a half-supplied pair into a startup error instead of a
/// silent fall-back to plain HTTP that a `--tls-cert`-only typo would
/// otherwise produce.
#[cfg(feature = "control")]
fn admin_tls_config(cli: &Cli) -> Result<Option<rustls::ServerConfig>> {
    match (&cli.tls_cert, &cli.tls_key) {
        (Some(cert), Some(key)) => Ok(Some(fastllm_proxy::control::tls::load_server_config(
            cert, key,
        )?)),
        (None, None) => Ok(None),
        _ => anyhow::bail!("--tls-cert and --tls-key must be given together; only one was set"),
    }
}

/// A missing or empty `--proxy-token` must never mean "`/snapshot` and
/// `/usage` accept anyone" (see `control::api::proxy_token_authorised`, and
/// the review finding it fixes: `constant_time_eq(b"", b"")` is `true`, so
/// `unwrap_or_default()` alone used to make an unset token equivalent to no
/// authentication at all). Between the two ways to make that safe — refuse
/// to start, or start with those two routes permanently closed — this
/// refuses to start.
///
/// `/snapshot` is how every `Http`-mode proxy in the deployment gets its
/// policy at all; permanently closing it would not fail loudly once, it
/// would silently starve every proxy of updates from that point on, which is
/// exactly the kind of quiet failure this whole batch of fixes exists to
/// stop introducing. A hard startup error is loud, immediate, and it does
/// not touch the one case that has to keep working with no token
/// configured: `File` mode (`--role proxy`, no `--control-url`) never calls
/// this at all — it has no admin API and needs no proxy token — so a
/// bare-YAML dev deployment is unaffected. The shipped manifests already set
/// a real token for `--role control`/`all` (see the review finding), so this
/// only turns an already-misconfigured deployment's silent open door into a
/// startup failure that says so.
#[cfg(feature = "control")]
fn require_proxy_token(cli: &Cli) -> Result<String> {
    cli.proxy_token.clone().filter(|t| !t.is_empty()).context(
        "--proxy-token (or FASTLLM_PROXY_TOKEN) is required for --role control/all and must \
             not be empty: it is what gates /snapshot (every key hash and usable upstream \
             credential) and /usage (a write route). Refusing to start with it unset is safer \
             than serving with those routes open to anyone.",
    )
}

/// `--role control`: database and admin API, no proxy listener at all.
///
/// Not gated behind `cfg(feature = "control")` at the call site — that would
/// make `--role control` silently do nothing on a `--no-default-features`
/// build. It fails loudly instead, at the one place that knows why.
async fn run_control(cli: Cli) -> Result<()> {
    #[cfg(feature = "control")]
    {
        // Loaded before the database connection: `--role control` always
        // reads (and, via the admin API, never writes) `upstream_api_key`,
        // so it needs the key exactly as unconditionally as `import` does.
        // See `EncryptionKey::from_env` for why a missing/malformed key is a
        // hard startup failure rather than a plaintext fallback.
        let key = Arc::new(fastllm_proxy::control::secrets::EncryptionKey::from_env()?);
        let proxy_token = require_proxy_token(&cli)?;
        let db_url = cli
            .database_url
            .clone()
            .context("--role control requires --database-url")?;
        // Registered before the first build so the very first snapshot carries
        // centroids — otherwise classes would be unroutable until the next
        // rebuild, which is the kind of gap nobody notices until a rule
        // silently stops matching after a restart.
        #[cfg(feature = "classifier")]
        register_snapshot_embedder(&cli);
        // Before the first build, for the same reason as the embedder above: a
        // backend whose credential has to be minted would otherwise be missing
        // from the very first snapshot, and unroutable until the next rebuild.
        register_token_minter(&cli)?;
        let pool = fastllm_proxy::control::db::connect(&db_url).await?;
        let snap = fastllm_proxy::control::build::build_snapshot(&pool, &key).await?;
        let cache: Arc<dyn fastllm_proxy::control::api::SnapshotSink> =
            Arc::new(ArcSwap::from_pointee(snap));
        fastllm_proxy::control::api::spawn_snapshot_rebuilder(
            pool.clone(),
            Arc::clone(&cache),
            Duration::from_secs(cli.snapshot_rebuild_interval),
            Arc::clone(&key),
        );
        // Both roles that own the database run this. `--role=control` is the
        // one deployed on the cluster, so omitting it here would have meant
        // the retention policy existed everywhere except in production.
        fastllm_proxy::control::api::spawn_usage_retention(pool.clone());
        let tls = admin_tls_config(&cli)?;
        let addr: SocketAddr = format!("{}:{}", cli.host, cli.admin_port)
            .parse()
            .with_context(|| {
                format!("invalid admin bind address {}:{}", cli.host, cli.admin_port)
            })?;
        info!(%addr, tls = tls.is_some(), "control plane admin API listening");
        let facts = deployment_facts(&cli, "control");
        // `--role=control` proxies nothing, so it has no upstream client of
        // its own; webhooks are its only outbound HTTP. One small pool.
        let notify_client = Arc::new(upstream::Upstream::new(
            upstream::Config {
                max_idle_per_host: 2,
                idle_timeout: Duration::from_secs(90),
                connect_timeout: Duration::from_secs(5),
            },
            tls_config(cli.ca_bundle.as_deref())?,
        ));
        let webhook = webhook_sender(&cli, &notify_client);
        fastllm_proxy::control::api::serve(pool, addr, proxy_token, cache, key, tls, facts, webhook)
            .await
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = cli;
        anyhow::bail!("--role control requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// `--role all`: the control plane and the proxy in one process, sharing one
/// `AppState`. `state` itself is handed to the admin API as its
/// `control::api::SnapshotSink` (see that trait, and `AppState::apply_snapshot`),
/// so a key or model write over the admin API reaches the routing `Registry`
/// through the exact same call the proxy's request path reads from — no HTTP
/// round trip back into the same process, no poll delay, and no separate
/// "also rebuild the registry" step to forget.
async fn run_all(cli: Cli) -> Result<()> {
    #[cfg(feature = "control")]
    {
        // See the matching comment in `run_control`: `--role all` reads
        // *and* writes `upstream_api_key` (admin API key/model writes go
        // through the same database), so it needs the key just as
        // unconditionally.
        let key = Arc::new(fastllm_proxy::control::secrets::EncryptionKey::from_env()?);
        let proxy_token = require_proxy_token(&cli)?;
        let db_url = cli
            .database_url
            .clone()
            .context("--role all requires --database-url")?;
        // Registered before the first build so the very first snapshot carries
        // centroids — otherwise classes would be unroutable until the next
        // rebuild, which is the kind of gap nobody notices until a rule
        // silently stops matching after a restart.
        #[cfg(feature = "classifier")]
        register_snapshot_embedder(&cli);

        debug!("startup: loading tuning config");
        let tuning_cfg = load_tuning_config(cli.config.as_ref())?;
        let tuning = tuning_cfg.fastllm.clone();
        let interner = Interner::default();
        // Built before the first snapshot, not after: minting a Vertex access
        // token needs it, and a backend whose credential could not be minted
        // is dropped from the snapshot it was being built for.
        let client = Arc::new(upstream::Upstream::new(
            upstream::Config {
                max_idle_per_host: cli.pool_max_idle,
                idle_timeout: Duration::from_secs(90),
                connect_timeout: Duration::from_secs(5),
            },
            tls_config(cli.ca_bundle.as_deref())?,
        ));
        fastllm_proxy::control::gcp::init(Arc::clone(&client));
        let pool = fastllm_proxy::control::db::connect(&db_url).await?;
        let snap = fastllm_proxy::control::build::build_snapshot(&pool, &key).await?;
        let master_key = cli
            .master_key
            .clone()
            .or_else(|| tuning_cfg.general_settings.master_key.clone())
            .filter(|k| !k.is_empty());
        if master_key.is_some() {
            // `--role all` gets its snapshot from `build_snapshot`, which
            // never merges a legacy master key in — that merge only happens
            // in `File`-mode `run_data_plane`. Storing the value on
            // `AppState` anyway (for `reload()`'s benefit, which `all` never
            // calls either) without saying so would leave an operator
            // believing a compat flag they set still works. It does not: a
            // fresh install must go through migration 0003's bootstrap
            // principal or the admin API instead.
            warn!(
                "--master-key has no effect in `--role all`; it is only honoured in `File` mode \
                 (--role proxy with --config and no --control-url). Mint keys through the admin \
                 API instead."
            );
        }

        // P3 wires `usage::record` in: `--role all` is one process, so
        // rather than inventing a second, in-process delivery path for
        // usage events, this loops a `POST /usage` request back to its own
        // admin API over loopback — the exact same wire protocol and the
        // exact same `budgets.tokens_used` increment (`control::api::post_usage`)
        // that an external `Http`-mode proxy already uses. `127.0.0.1`
        // specifically, not `cli.host`: this is always same-host by
        // construction, and `cli.host` may be `0.0.0.0`, which is not a
        // valid address to *connect* to.
        debug!("startup: loading admin TLS config");
        let admin_tls = admin_tls_config(&cli)?;
        let admin_scheme = if admin_tls.is_some() { "https" } else { "http" };
        debug!("startup: spawning usage reporter");
        let usage = fastllm_proxy::usage::spawn(
            fastllm_proxy::usage::ReporterConfig {
                url: format!("{admin_scheme}://127.0.0.1:{}/usage", cli.admin_port),
                token: proxy_token.clone(),
                queue_capacity: 10_000,
                batch_max: 500,
                flush_interval: Duration::from_secs(5),
            },
            Arc::clone(&client),
        );

        // Before `client` is moved into the app state below, since the
        // notifier shares the same connection pool.
        let admin_webhook = webhook_sender(&cli, &client);
        let state = build_app_state(
            &cli, client, interner, &tuning, None, master_key, snap, usage,
        )?;
        // `all` reports to its own admin API over loopback, the same path
        // usage takes, rather than inventing a second in-process route.
        spawn_health_reports(
            Arc::clone(&state),
            fastllm_proxy::health_report::spawn(
                fastllm_proxy::health_report::Config {
                    url: format!(
                        "{admin_scheme}://127.0.0.1:{}/health-report",
                        cli.admin_port
                    ),
                    token: proxy_token.clone(),
                    interval: Duration::from_secs(cli.health_report_interval),
                },
                Arc::clone(&state.client),
            ),
            Duration::from_secs(cli.health_report_interval),
        );

        let admin_addr: SocketAddr = format!("{}:{}", cli.host, cli.admin_port)
            .parse()
            .with_context(|| {
                format!("invalid admin bind address {}:{}", cli.host, cli.admin_port)
            })?;
        let admin_pool = pool.clone();
        let admin_sink = Arc::clone(&state) as Arc<dyn fastllm_proxy::control::api::SnapshotSink>;
        let admin_facts = deployment_facts(&cli, "all");
        let admin_token = proxy_token;
        fastllm_proxy::control::api::spawn_snapshot_rebuilder(
            pool.clone(),
            Arc::clone(&admin_sink),
            Duration::from_secs(cli.snapshot_rebuild_interval),
            Arc::clone(&key),
        );
        // Usage now takes a row per request rather than only for capped
        // principals, so the table needs a policy rather than watching.
        fastllm_proxy::control::api::spawn_usage_retention(pool.clone());
        let admin_tls_enabled = admin_tls.is_some();
        tokio::spawn(async move {
            if let Err(e) = fastllm_proxy::control::api::serve(
                admin_pool,
                admin_addr,
                admin_token,
                admin_sink,
                key,
                admin_tls,
                admin_facts,
                admin_webhook,
            )
            .await
            {
                error!(error = %e, "control plane admin API exited");
            }
        });
        info!(%admin_addr, tls = admin_tls_enabled, "control plane admin API listening");

        serve_proxy(&cli, state).await
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = cli;
        anyhow::bail!("--role all requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// `--role proxy`: forwarding only, against a config file (`File` mode) or a
/// control plane (`Http` mode). Builds and owns its own snapshot cell, and
/// *does* spawn a poller — that poller, and SIGHUP in `File` mode, are what
/// makes editing keys or grants live rather than requiring a restart.
async fn run_data_plane(cli: Cli) -> Result<()> {
    let tuning_cfg = load_tuning_config(cli.config.as_ref())?;
    let tuning = tuning_cfg.fastllm.clone();
    let interner = Interner::default();

    let client = Arc::new(upstream::Upstream::new(
        upstream::Config {
            max_idle_per_host: cli.pool_max_idle,
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(5),
        },
        tls_config(cli.ca_bundle.as_deref())?,
    ));

    // Deprecated: a single shared key is exactly what this release replaces,
    // but silently breaking a running deployment is worse than a warning.
    // Whether it actually *does* anything depends on which mode this turns
    // out to be — see the mode-specific warning below, once that is known.
    let master_key = cli
        .master_key
        .clone()
        .or_else(|| tuning_cfg.general_settings.master_key.clone())
        .filter(|k| !k.is_empty());

    enum Mode {
        /// No control plane. SIGHUP and a poll both apply.
        File,
        /// Polls a control plane; falls back to its disk cache (and then to
        /// empty) if that control plane is unreachable at startup.
        Http { url: String, token: String },
    }

    // `disabled()` unless `Http` mode sets up the real thing below — `File`
    // mode has no control plane to report usage to at all. See
    // `usage::UsageReporter::disabled`'s doc comment for why this is the
    // same type either way rather than an `Option`.
    let mut usage = fastllm_proxy::usage::UsageReporter::disabled();

    let (snap, mode, config_path): (Snapshot, Mode, Option<PathBuf>) = if let Some(url) =
        cli.control_url.clone()
    {
        let token = cli.proxy_token.clone().unwrap_or_default();
        let http_src = HttpSource::new(
            url.clone(),
            token.clone(),
            cli.snapshot_cache.clone(),
            Arc::clone(&client),
        );
        // Spawned before the first request can arrive, so `/metrics`' drop
        // counter is meaningful from the start rather than from whenever the
        // first event happens to be recorded.
        usage = fastllm_proxy::usage::spawn(
            fastllm_proxy::usage::ReporterConfig {
                url: usage_url(&url),
                token: token.clone(),
                queue_capacity: 10_000,
                batch_max: 500,
                flush_interval: Duration::from_secs(5),
            },
            Arc::clone(&client),
        );
        // A proxy that starts with nothing must still start. Refusing to
        // boot would turn a control-plane outage into a crash-loop, which
        // is exactly the failure this architecture exists to prevent.
        let snap = match http_src.fetch(None).await {
            Ok(Some(s)) => s,
            Ok(None) => http_src.load_cached().unwrap_or_default(),
            Err(e) => {
                warn!(error = %e, "control plane unreachable at startup; falling back to the disk cache");
                http_src.load_cached().unwrap_or_default()
            }
        };
        if master_key.is_some() {
            // Unlike `File` mode, nothing here ever calls
            // `add_legacy_master_key`: `Http` mode's snapshot comes from the
            // control plane's `/snapshot`, and the poller below polls it with
            // a plain `HttpSource`, which carries no such key.
            // A flag that is silently stored and never consulted is worse
            // than one that says so — see Task 12's final review.
            warn!(
                "--master-key has no effect in `Http` mode (--control-url set); it is only \
                 honoured in `File` mode (--config, no --control-url). Define keys through the \
                 control plane's admin API instead."
            );
        }
        (snap, Mode::Http { url, token }, cli.config.clone())
    } else {
        let path = cli
            .config
            .clone()
            .context("File mode (no --control-url) requires --config")?;
        // Deprecated, but this is the one mode where it actually still works
        // (merged in by `FileSource` on every fetch, not just this
        // first one) — see the `Http`/`all` branches for why the same flag is
        // inert everywhere else.
        if master_key.is_some() {
            warn!("--master-key is deprecated; define keys under `auth:` instead");
        }
        let file_src = FileSource::new(path.clone()).with_legacy_master_key(master_key.clone());
        let snap = file_src
            .fetch(None)
            .await?
            .context("config produced no snapshot on first load")?;
        (snap, Mode::File, Some(path))
    };

    let state = build_app_state(
        &cli,
        client,
        interner,
        &tuning,
        config_path,
        master_key,
        snap,
        usage,
    )?;

    match mode {
        Mode::File => {
            spawn_reload_listener(Arc::clone(&state));
            if cli.config_poll > 0 {
                let path = state
                    .config_path
                    .clone()
                    .expect("File mode always sets config_path");
                let source =
                    FileSource::new(path).with_legacy_master_key(state.legacy_master_key.clone());
                spawn_poller(
                    source,
                    Arc::clone(&state),
                    Duration::from_secs(cli.config_poll),
                );
            }
        }
        Mode::Http { url, token } => {
            // SIGHUP has no defined job here: re-reading `--config` would
            // reload the wrong source of truth. The poll interval is the
            // only reload path, same as any other control-plane consumer.
            if cli.config_poll > 0 {
                let source = HttpSource::new(
                    url.clone(),
                    token.clone(),
                    cli.snapshot_cache.clone(),
                    Arc::clone(&state.client),
                );
                spawn_poller(
                    source,
                    Arc::clone(&state),
                    Duration::from_secs(cli.config_poll),
                );
            }
            // Backend health only exists in the data plane, and a management
            // UI has no other way to ask for it. Same reverse channel and same
            // token as usage.
            spawn_health_reports(
                Arc::clone(&state),
                fastllm_proxy::health_report::spawn(
                    fastllm_proxy::health_report::Config {
                        url: health_report_url(&url),
                        token: token.clone(),
                        interval: Duration::from_secs(cli.health_report_interval),
                    },
                    Arc::clone(&state.client),
                ),
                Duration::from_secs(cli.health_report_interval),
            );
            // P2 reconciliation: only `Http`-mode `--role proxy` has a
            // control plane to reconcile with at all. See
            // `crate::reconcile`'s doc comment for why `File` mode and
            // `--role all` never reach this branch.
            if cli.rate_limit_reconcile_interval > 0 {
                fastllm_proxy::reconcile::spawn(
                    fastllm_proxy::reconcile::ReconcileConfig {
                        url: reconcile_url(&url),
                        token,
                        replica_id: replica_id(),
                        interval: Duration::from_secs(cli.rate_limit_reconcile_interval),
                    },
                    Arc::clone(&state.limiter),
                    Arc::clone(&state.client),
                );
            }
        }
    }

    serve_proxy(&cli, state).await
}

/// Build the shared process state from an already-obtained initial snapshot.
/// Common to `--role all` and `--role proxy`, so there is exactly one place
/// that turns a snapshot into a `Registry` and logs what was loaded.
// One more parameter (`usage`) than clippy's default threshold. Bundling
// these into a params struct would help nothing here: every field is a
// distinct, differently-sourced piece of already-resolved startup state
// (parsed CLI, a pooled client, a loaded snapshot...), not a cohesive value
// that belongs together, and grouping them into a temporary struct just to
// make one field count smaller replaces this warning with a struct nobody
// else uses.
#[allow(clippy::too_many_arguments)]
fn build_app_state(
    cli: &Cli,
    client: fastllm_proxy::state::HttpClient,
    interner: Interner,
    tuning: &fastllm_proxy::config::FastllmSettings,
    config_path: Option<PathBuf>,
    legacy_master_key: Option<String>,
    snapshot: Snapshot,
    usage: fastllm_proxy::usage::UsageReporter,
) -> Result<Arc<AppState>> {
    if snapshot.open {
        warn!("no keys configured; the proxy accepts unauthenticated requests");
    }
    let registry = Registry::build_from_snapshot(&snapshot, &interner, None)?;
    if registry.backends().is_empty() {
        warn!("no backends in the snapshot; every request will 404 until it is reloaded");
    }
    info!(
        models = registry.model_names().len(),
        backends = registry.backends().len(),
        policy = ?cli.policy,
        role = ?cli.role,
        "starting"
    );

    #[cfg(feature = "classifier")]
    let tier1 = match cli.classifier_model.as_deref() {
        None => None,
        Some(source) => match fastllm_proxy::classifier::tier1::Tier1::load(source) {
            Ok(t) => {
                info!(model = %source, "fast-tier classifier loaded");
                Some(Arc::new(t))
            }
            Err(e) => {
                // Not fatal: a proxy that cannot classify still serves every
                // request, it simply cannot match a rule that names a class.
                // Refusing to start would turn a classifier problem into an
                // outage.
                // `{:#}` prints the whole anyhow chain: the outer context
                // says which model, the inner cause says *why*, and it was the
                // inner one ("permission denied") that identified a Dockerfile
                // bug the outer message alone could not.
                warn!(error = format!("{e:#}"), model = %source, "fast-tier classifier failed \
                    to load; rules naming a prompt class will not match");
                None
            }
        },
    };

    let state = Arc::new(AppState {
        registry: ArcSwap::from_pointee(registry),
        router: Router::new(
            cli.policy,
            tuning.affinity_slots,
            tuning.prefix_bytes,
            tuning.balance_abs,
            tuning.balance_rel,
        ),
        client,
        interner,
        config_path,
        legacy_master_key,
        snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
        max_body_bytes: cli.max_body_mb.saturating_mul(1024 * 1024),
        max_retries: cli.max_retries,
        #[cfg(feature = "classifier")]
        classifier: ArcSwap::from_pointee(fastllm_proxy::classifier::Classifier::default()),
        #[cfg(feature = "classifier")]
        tier1,
        #[cfg(feature = "classifier-tier2")]
        tier2_path: cli.classifier_tier2_model.clone(),
        #[cfg(feature = "classifier-tier2")]
        tier2: std::sync::OnceLock::new(),
        upstream_headers_timeout: Duration::from_secs(cli.upstream_timeout),
        unhealthy_after: tuning.unhealthy_after.max(1),
        started: Instant::now(),
        requests_ok: AtomicU64::new(0),
        requests_failed: AtomicU64::new(0),
        usage,
        limiter: Arc::new(fastllm_proxy::limiter::Limiter::new()),
        telemetry: Arc::new(fastllm_proxy::telemetry::Telemetry::new()),
        cache: Arc::new(fastllm_proxy::cache::ResponseCache::new(
            cli.cache_max_entries,
            cli.cache_max_bytes,
        )),
    });

    // The constructor above bypasses `apply_snapshot`, so anything that path
    // derives has to be primed once here. See `AppState::prime_derived_views`
    // for what went wrong when it was not.
    state.prime_derived_views();
    // A proxy that starts with escalation already configured must not charge
    // the model load to its first escalating request.
    #[cfg(feature = "classifier-tier2")]
    state.warm_refined_tier();
    Ok(state)
}

/// Derive `POST /usage`'s URL from `--control-url`, which names
/// `/snapshot` (see `deploy/deployment.yaml`'s `FASTLLM_CONTROL_URL`): the
/// two are sibling routes on the same control plane, so this avoids a second
/// flag that could point somewhere else and drift out of sync with the first.
/// `POST /health-report`'s URL, derived the same way `usage_url` derives
/// `/usage`'s: a sibling route on the same control plane, from the one
/// `--control-url` flag rather than a second that could drift.
fn health_report_url(control_url: &str) -> String {
    format!(
        "{}/health-report",
        control_url.strip_suffix("/snapshot").unwrap_or(control_url)
    )
}

/// Report what this proxy can see, on a timer.
///
/// A timer rather than on-change: health is a level, not an event, and a UI
/// polling the control plane wants the current value rather than a
/// reconstruction from a stream of transitions it may have missed.
///
/// Entirely off the request path. Reading the registry is the same cheap load
/// `/health` already does, and the send is a background task that drops rather
/// than blocks.
fn spawn_health_reports(
    state: Arc<fastllm_proxy::state::AppState>,
    reporter: fastllm_proxy::health_report::Reporter,
    interval: Duration,
) {
    let replica = hostname();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let registry = state.registry.load();
            reporter.send(fastllm_proxy::health_report::HealthReport {
                replica: replica.clone(),
                snapshot_version: state.snapshot.load().version,
                uptime_seconds: state.started.elapsed().as_secs(),
                process: fastllm_proxy::health_report::ProcessCounters {
                    requests_ok: state.requests_ok.load(std::sync::atomic::Ordering::Relaxed),
                    requests_failed: state
                        .requests_failed
                        .load(std::sync::atomic::Ordering::Relaxed),
                    cache_hits: state.cache.hits.load(std::sync::atomic::Ordering::Relaxed),
                    cache_misses: state
                        .cache
                        .misses
                        .load(std::sync::atomic::Ordering::Relaxed),
                    cache_entries: state.cache.len(),
                    cache_bytes: state.cache.bytes(),
                    usage_dropped: state.usage.dropped(),
                    rejected_unauthenticated: state
                        .telemetry
                        .rejections_of(fastllm_proxy::telemetry::Rejection::Unauthenticated),
                    rejected_model_not_found: state
                        .telemetry
                        .rejections_of(fastllm_proxy::telemetry::Rejection::ModelNotFound),
                },
                backends: registry
                    .backends()
                    .iter()
                    .map(|b| fastllm_proxy::health_report::BackendHealth {
                        api_base: b.api_base.clone(),
                        model: b.upstream_model.clone(),
                        healthy: b.is_healthy(),
                        inflight: b.inflight(),
                        requests_total: b.requests_total(),
                        errors_total: b.errors_total(),
                    })
                    .collect(),
            });
        }
    });
}

/// Which replica this is. The pod name under Kubernetes, which is what an
/// operator would `kubectl logs` next.
fn hostname() -> String {
    if let Some(h) = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()) {
        return h;
    }
    // Kubernetes always sets HOSTNAME to the pod name, which is the thing an
    // operator would `kubectl logs` next, so this branch is for running the
    // binary by hand. It carries the pid because the fleet is keyed on this
    // string: a constant fallback would make every replica overwrite the same
    // entry, and a fleet of six would silently read as one.
    format!("unknown-{}", std::process::id())
}

fn usage_url(control_url: &str) -> String {
    format!(
        "{}/usage",
        control_url.strip_suffix("/snapshot").unwrap_or(control_url)
    )
}

/// Derive `POST /limits/reconcile`'s URL the same way `usage_url` derives
/// `/usage`'s -- a sibling route on the same control plane, from the one
/// `--control-url` flag, rather than a second flag that could drift.
fn reconcile_url(control_url: &str) -> String {
    format!(
        "{}/limits/reconcile",
        control_url.strip_suffix("/snapshot").unwrap_or(control_url)
    )
}

/// A per-process identifier for P2 reconciliation
/// (`crate::control::reconcile::ReconcileState` keys reports by this), not
/// parsed by anything -- only ever used as a map key on the control plane,
/// so uniqueness for this process's lifetime is all that is required. Built
/// from the PID plus a startup timestamp rather than pulling in a UUID
/// dependency (or `rand`, which is behind the `control` feature and
/// unavailable to a `--no-default-features` data-plane build) for a value
/// with no other purpose.
fn replica_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    // The counter, not just the timestamp, is what guarantees distinctness:
    // `main.rs` only ever calls this once per process, but a low-resolution
    // system clock could otherwise produce the same nanosecond value twice
    // in a tight loop (as the unit test below does).
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// Health sweeps and the proxy listener's accept loop. Common to `--role
/// all` and `--role proxy` — everything role-specific (building `state`,
/// wiring a poller/SIGHUP/the admin API) has already happened by the time
/// this is called.
async fn serve_proxy(cli: &Cli, state: Arc<AppState>) -> Result<()> {
    health::spawn(
        Arc::clone(&state),
        Duration::from_secs(cli.health_interval.max(1)),
        Duration::from_secs(cli.health_timeout.max(1)),
    );
    // Unconditional, unlike `reconcile::spawn` -- every role that reaches
    // this function owns a `Limiter` and can accumulate idle entries in it
    // (a deleted principal, a rotated key's old principal), regardless of
    // whether it also happens to be reconciling shares with a control
    // plane. See `limiter::Limiter::evict_idle`'s doc comment for why this
    // is safe to run on its own schedule.
    fastllm_proxy::limiter::spawn_eviction(Arc::clone(&state.limiter));

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", cli.host, cli.port))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    if !addr.ip().is_loopback() {
        warn!(%addr, "bound to a non-loopback address; ensure the master key is set");
    }
    info!(%addr, "fastllm-proxy listening");

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // Told once, on signal: stop keep-alive and finish what is in flight.
    // Without it a drain would wait the full grace period every time, because
    // an idle keep-alive connection is indistinguishable from a busy one by
    // counting alone.
    let (close_tx, close_rx) = tokio::sync::watch::channel(false);
    let live = Arc::new(AtomicUsize::new(0));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        error!(error = %e, "accept failed");
                        continue;
                    }
                };
                if let Err(e) = stream.set_nodelay(true) {
                    warn!(error = %e, "could not set TCP_NODELAY on {peer}");
                }
                let state = Arc::clone(&state);
                let live = Arc::clone(&live);
                let mut close_rx = close_rx.clone();
                live.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let service = service_fn(move |req| proxy::handle(req, Arc::clone(&state)));
                    let conn = http1::Builder::new()
                        .keep_alive(true)
                        // Let the client stop uploading while the response is
                        // still streaming, and vice versa.
                        .half_close(true)
                        .serve_connection(TokioIo::new(stream), service);
                    let mut conn = std::pin::pin!(conn);
                    loop {
                        tokio::select! {
                            res = conn.as_mut() => {
                                if let Err(e) = res {
                                    // Disconnects mid-generation are routine,
                                    // not errors.
                                    tracing::debug!(error = %e, "connection closed");
                                }
                                break;
                            }
                            // Fires once. `graceful_shutdown` stops this
                            // connection accepting another request on the same
                            // socket, but leaves a response already streaming
                            // alone — which is the whole point: a generation
                            // that has been running for a minute should not be
                            // severed because a rollout started.
                            _ = close_rx.changed() => {
                                conn.as_mut().graceful_shutdown();
                            }
                        }
                    }
                    live.fetch_sub(1, Ordering::Relaxed);
                });
            }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                break;
            }
        }
    }

    drain(&close_tx, &live, Duration::from_secs(cli.shutdown_grace)).await;
    Ok(())
}

/// Let in-flight requests finish, up to `grace`.
///
/// Kubernetes sends SIGTERM and then SIGKILLs at `terminationGracePeriodSeconds`
/// (30 by default). Before this, the signal broke the accept loop and the
/// process fell out of `main`, killing every connection task mid-stream — so a
/// rollout truncated whatever generations were running, and a client saw a
/// response stop mid-sentence with no error to retry on.
///
/// Polled rather than notified: this runs once per process lifetime, and a
/// 100ms poll has none of the wakeup races a condvar here would need to get
/// right for no measurable gain.
async fn drain(
    close_tx: &tokio::sync::watch::Sender<bool>,
    live: &Arc<AtomicUsize>,
    grace: Duration,
) {
    let _ = close_tx.send(true);
    if grace.is_zero() {
        return;
    }
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        let remaining = live.load(Ordering::Relaxed);
        if remaining == 0 {
            info!("all connections closed; shutting down");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            // Said out loud: these are requests a client is still waiting on,
            // and they are about to be cut. Silence here would make a truncated
            // response look like a client-side bug.
            warn!(
                connections = remaining,
                grace_seconds = grace.as_secs(),
                "shutdown grace expired with connections still open; closing them"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Root certificates for `https://` api_bases, and for `https://` control
/// plane URLs (`--control-url`) — both go through the one shared `Upstream`
/// (see its doc comment), so both trust whatever this builds.
///
/// The typical deployment is plain HTTP to nodes on a private network, but the
/// config schema accepts `https://` and a hosted or TLS-terminated endpoint is
/// a legitimate backend — accepting the URL and then failing to connect is the
/// worst of both.
///
/// System roots are preferred so an internal CA already trusted by the host
/// works without extra configuration; the bundled Mozilla set is the fallback
/// for minimal containers that ship no root store at all. `ca_bundle` adds to
/// that rather than replacing it, so an operator trusting a private
/// cert-manager CA for the control plane does not also lose the ability to
/// reach a public, publicly-CA-signed backend.
fn tls_config(ca_bundle: Option<&std::path::Path>) -> Result<rustls::ClientConfig> {
    debug!("startup: building rustls client config (loading root certificates)");
    // Exactly one provider is compiled in, but installing it explicitly keeps
    // this deterministic rather than dependent on rustls' defaulting rules.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    match rustls_native_certs::load_native_certs() {
        result if !result.certs.is_empty() => {
            for cert in result.certs {
                let _ = roots.add(cert);
            }
        }
        _ => {
            warn!("no system root certificates; falling back to the bundled Mozilla set");
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }

    if let Some(path) = ca_bundle {
        let added = load_extra_ca_bundle(&mut roots, path)
            .with_context(|| format!("loading --ca-bundle {}", path.display()))?;
        info!(
            certs = added,
            path = %path.display(),
            "extra CA bundle trusted, in addition to the system root store"
        );
    }

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Without this the handshake offers no ALPN and a strict server closes the
    // connection rather than guessing — which is exactly what api.openai.com
    // did, reported as "closed before sending a response". The old
    // hyper-rustls builder set this implicitly via enable_http1().
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Parse a PEM file that may hold one or more CA certificates (a bundle) and
/// add every one of them to `roots`. Returns how many were added, so the
/// caller can log a number rather than just "loaded" — a bundle with zero
/// usable certs (wrong file, wrong format) is a misconfiguration worth
/// noticing at startup rather than a silent no-op.
fn load_extra_ca_bundle(
    roots: &mut rustls::RootCertStore,
    path: &std::path::Path,
) -> Result<usize> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut added = 0;
    for cert in rustls_pemfile::certs(&mut reader) {
        roots.add(cert?)?;
        added += 1;
    }
    if added == 0 {
        anyhow::bail!("no PEM certificates found in {}", path.display());
    }
    Ok(added)
}

/// SIGHUP reloads the config in place, in `File` mode.
///
/// This is the whole point of owning the process: the model set changes when a
/// workload is launched or stopped, and applying that should not mean tearing
/// down a gateway that has generations in flight. `main.rs` only calls this
/// for `File`-mode `run_data_plane`; see `AppState::reload`'s doc comment for
/// why `Http` and `all` deployments do not wire it up.
///
/// The interval-based watch this used to leave to a separate function
/// (`spawn_config_watcher`) is now `source::spawn_poller` on the same
/// `FileSource`: one hash-based change check that updates the snapshot *and*
/// rebuilds the registry, instead of two independent reload paths that could
/// disagree about what "changed" means. SIGHUP still exists alongside it
/// purely to apply an edit immediately rather than waiting for the next tick.
fn spawn_reload_listener(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut hangup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "cannot listen for SIGHUP; config reload disabled");
                return;
            }
        };
        while hangup.recv().await.is_some() {
            match state.reload().await {
                Ok(n) => info!(backends = n, "config reloaded"),
                Err(e) => error!(error = %e, "config reload failed; keeping previous config"),
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Builds prompt-class centroids for the control plane.
///
/// Lives here rather than in `control::build` because it is the one place both
/// the `control` and `classifier` features are known to be present; `build`
/// itself must compile without a classifier and simply publish classes with no
/// centroid.
#[cfg(all(feature = "control", feature = "classifier"))]
struct SnapshotEmbedder {
    tier1: Option<Arc<fastllm_proxy::classifier::tier1::Tier1>>,
    #[cfg(feature = "classifier-tier2")]
    tier2_path: Option<String>,
    #[cfg(feature = "classifier-tier2")]
    tier2: std::sync::OnceLock<Option<Arc<fastllm_proxy::classifier::tier2::Tier2>>>,
}

#[cfg(all(feature = "control", feature = "classifier"))]
impl fastllm_proxy::control::build::PromptClassEmbedder for SnapshotEmbedder {
    fn fast(&self, prompts: &[String]) -> Option<Vec<Vec<f32>>> {
        Some(self.tier1.as_ref()?.embed_batch(prompts))
    }

    #[cfg(feature = "classifier-tier2")]
    fn refined(&self, prompts: &[String]) -> Option<Vec<Vec<f32>>> {
        // Loaded on demand here too: a control plane whose operator defined no
        // refined classes never touches the transformer either.
        let loaded = self.tier2.get_or_init(|| {
            let path = self.tier2_path.as_ref()?;
            match fastllm_proxy::classifier::tier2::Tier2::load(path) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    warn!(error = %e, path = %path, "refined-tier classifier failed to load");
                    None
                }
            }
        });
        loaded.as_ref()?.embed_batch(prompts)
    }

    #[cfg(not(feature = "classifier-tier2"))]
    fn refined(&self, _prompts: &[String]) -> Option<Vec<Vec<f32>>> {
        None
    }
}

/// Load the classifier models and hand them to `control::build`.
///
/// Separate from the data plane's own `tier1`: `--role control` never serves a
/// request, so it needs the model only to average example prompts into
/// centroids, and `--role all` ends up loading it twice. Two copies of a 61MB
/// table is the cheaper mistake than sharing one across a boundary the two
/// roles otherwise keep clean.
/// Give the control plane the HTTP client it needs to mint access tokens.
///
/// `--role control` proxies nothing, so it has no client of its own; this is
/// the one it gets. `--role all` shares the client it already has rather than
/// standing up a second pool.
#[cfg(feature = "control")]
fn register_token_minter(cli: &Cli) -> Result<()> {
    let client = Arc::new(upstream::Upstream::new(
        upstream::Config {
            max_idle_per_host: cli.pool_max_idle,
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(5),
        },
        tls_config(cli.ca_bundle.as_deref())?,
    ));
    fastllm_proxy::control::gcp::init(client);
    Ok(())
}

#[cfg(all(feature = "control", feature = "classifier"))]
fn register_snapshot_embedder(cli: &Cli) {
    let Some(source) = cli.classifier_model.as_deref() else {
        return;
    };
    let tier1 = match fastllm_proxy::classifier::tier1::Tier1::load(source) {
        Ok(t) => Some(Arc::new(t)),
        Err(e) => {
            warn!(error = format!("{e:#}"), model = %source, "classifier model failed to load; \
                prompt classes will be published without centroids and cannot match");
            None
        }
    };
    fastllm_proxy::control::build::set_prompt_class_embedder(Box::new(SnapshotEmbedder {
        tier1,
        #[cfg(feature = "classifier-tier2")]
        tier2_path: cli.classifier_tier2_model.clone(),
        #[cfg(feature = "classifier-tier2")]
        tier2: std::sync::OnceLock::new(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_url_is_a_sibling_of_the_snapshot_url() {
        assert_eq!(
            usage_url("https://fastllm-control.fastllm.svc:4001/snapshot"),
            "https://fastllm-control.fastllm.svc:4001/usage"
        );
    }

    /// `--control-url` without a `/snapshot` suffix (a bare base URL) is
    /// still handled: the `/usage` route is simply appended.
    #[test]
    fn usage_url_tolerates_a_bare_base_url() {
        assert_eq!(
            usage_url("https://fastllm-control.fastllm.svc:4001"),
            "https://fastllm-control.fastllm.svc:4001/usage"
        );
    }

    #[test]
    fn reconcile_url_is_a_sibling_of_the_snapshot_url() {
        assert_eq!(
            reconcile_url("https://fastllm-control.fastllm.svc:4001/snapshot"),
            "https://fastllm-control.fastllm.svc:4001/limits/reconcile"
        );
    }

    #[test]
    fn reconcile_url_tolerates_a_bare_base_url() {
        assert_eq!(
            reconcile_url("https://fastllm-control.fastllm.svc:4001"),
            "https://fastllm-control.fastllm.svc:4001/limits/reconcile"
        );
    }

    #[test]
    fn replica_ids_are_distinct_across_calls() {
        assert_ne!(replica_id(), replica_id());
    }
}
