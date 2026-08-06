//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use fastllm_proxy::config::FileConfig;
use fastllm_proxy::registry::{Interner, Registry};
use fastllm_proxy::router::{Policy, Router};
use fastllm_proxy::snapshot::Snapshot;
use fastllm_proxy::source::file::FileSource;
use fastllm_proxy::source::http::HttpSource;
use fastllm_proxy::source::{spawn_poller, SnapshotSource, WithLegacyMasterKey};
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&cli.log)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(n) = cli.workers {
        builder.worker_threads(n.max(1));
    }
    builder.build()?.block_on(run(cli))
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
    }
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
        let db_url = cli
            .database_url
            .clone()
            .context("--role control requires --database-url")?;
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
        let tls = admin_tls_config(&cli)?;
        let addr: SocketAddr = format!("{}:{}", cli.host, cli.admin_port)
            .parse()
            .with_context(|| {
                format!("invalid admin bind address {}:{}", cli.host, cli.admin_port)
            })?;
        info!(%addr, tls = tls.is_some(), "control plane admin API listening");
        fastllm_proxy::control::api::serve(
            pool,
            addr,
            cli.proxy_token.unwrap_or_default(),
            cache,
            key,
            tls,
        )
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
        let db_url = cli
            .database_url
            .clone()
            .context("--role all requires --database-url")?;
        let pool = fastllm_proxy::control::db::connect(&db_url).await?;
        let snap = fastllm_proxy::control::build::build_snapshot(&pool, &key).await?;

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

        // `disabled()`: see `usage::UsageReporter::disabled`'s doc comment
        // for why `--role all` does not loop a `POST /usage` HTTP request
        // back into its own process here — that decision belongs to whatever
        // P2 wiring actually calls `record`, not to this scaffolding.
        let state = build_app_state(
            &cli,
            client,
            interner,
            &tuning,
            None,
            master_key,
            snap,
            fastllm_proxy::usage::UsageReporter::disabled(),
        )?;

        let admin_addr: SocketAddr = format!("{}:{}", cli.host, cli.admin_port)
            .parse()
            .with_context(|| {
                format!("invalid admin bind address {}:{}", cli.host, cli.admin_port)
            })?;
        let admin_pool = pool.clone();
        let admin_sink = Arc::clone(&state) as Arc<dyn fastllm_proxy::control::api::SnapshotSink>;
        let admin_token = cli.proxy_token.clone().unwrap_or_default();
        let admin_tls = admin_tls_config(&cli)?;
        fastllm_proxy::control::api::spawn_snapshot_rebuilder(
            pool.clone(),
            Arc::clone(&admin_sink),
            Duration::from_secs(cli.snapshot_rebuild_interval),
            Arc::clone(&key),
        );
        let admin_tls_enabled = admin_tls.is_some();
        tokio::spawn(async move {
            if let Err(e) = fastllm_proxy::control::api::serve(
                admin_pool,
                admin_addr,
                admin_token,
                admin_sink,
                key,
                admin_tls,
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
        // Nothing calls `usage.record(...)` yet (see `crate::usage`'s doc
        // comment — P2 wires token counting in), but the reporter is spawned
        // now so `/metrics`' drop counter is meaningful from the start and P2
        // has nothing left to plumb beyond calling `record`.
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
            // a plain `HttpSource`, not a `WithLegacyMasterKey`-wrapped one.
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
        // (merged in by `WithLegacyMasterKey` on every fetch, not just this
        // first one) — see the `Http`/`all` branches for why the same flag is
        // inert everywhere else.
        if master_key.is_some() {
            warn!("--master-key is deprecated; define keys under `auth:` instead");
        }
        let file_src = WithLegacyMasterKey::new(FileSource::new(path.clone()), master_key.clone());
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
                let source = WithLegacyMasterKey::new(
                    FileSource::new(path),
                    state.legacy_master_key.clone(),
                );
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
                    url,
                    token,
                    cli.snapshot_cache.clone(),
                    Arc::clone(&state.client),
                );
                spawn_poller(
                    source,
                    Arc::clone(&state),
                    Duration::from_secs(cli.config_poll),
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

    Ok(Arc::new(AppState {
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
        upstream_headers_timeout: Duration::from_secs(cli.upstream_timeout),
        unhealthy_after: tuning.unhealthy_after.max(1),
        started: Instant::now(),
        requests_ok: AtomicU64::new(0),
        requests_failed: AtomicU64::new(0),
        usage,
    }))
}

/// Derive `POST /usage`'s URL from `--control-url`, which names
/// `/snapshot` (see `deploy/deployment.yaml`'s `FASTLLM_CONTROL_URL`): the
/// two are sibling routes on the same control plane, so this avoids a second
/// flag that could point somewhere else and drift out of sync with the first.
fn usage_url(control_url: &str) -> String {
    format!(
        "{}/usage",
        control_url.strip_suffix("/snapshot").unwrap_or(control_url)
    )
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
                tokio::spawn(async move {
                    let service = service_fn(move |req| proxy::handle(req, Arc::clone(&state)));
                    let conn = http1::Builder::new()
                        .keep_alive(true)
                        // Let the client stop uploading while the response is
                        // still streaming, and vice versa.
                        .half_close(true)
                        .serve_connection(TokioIo::new(stream), service);
                    if let Err(e) = conn.await {
                        // Disconnects mid-generation are routine, not errors.
                        tracing::debug!(error = %e, "connection closed");
                    }
                });
            }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                break;
            }
        }
    }

    Ok(())
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
}
