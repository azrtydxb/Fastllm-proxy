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
    /// Path to the model config. LiteLLM-format configs are accepted as-is.
    #[arg(short, long, env = "FASTLLM_CONFIG")]
    config: PathBuf,

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

    /// Control plane and forwarding in one process (default), the admin API
    /// and database only, or forwarding only against a control plane or a
    /// config file.
    #[arg(long, value_enum, default_value_t = Role::All, env = "FASTLLM_ROLE")]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// Control plane and forwarding in one process. The default, and what a
    /// single container runs.
    All,
    /// Database, admin API and `/snapshot` only — no proxy listener.
    Control,
    /// Forwarding only, against a control plane or a config file.
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
    match cli.role {
        Role::Control => run_control(cli).await,
        Role::Proxy => run_data_plane(cli, None).await,
        Role::All => run_all(cli).await,
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
        let db_url = cli
            .database_url
            .clone()
            .context("--role control requires --database-url")?;
        let pool = fastllm_proxy::control::db::connect(&db_url).await?;
        let snap = fastllm_proxy::control::build::build_snapshot(&pool).await?;
        let cache = Arc::new(ArcSwap::from_pointee(snap));
        let addr: SocketAddr = format!("{}:{}", cli.host, cli.admin_port)
            .parse()
            .with_context(|| {
                format!("invalid admin bind address {}:{}", cli.host, cli.admin_port)
            })?;
        info!(%addr, "control plane admin API listening");
        fastllm_proxy::control::api::serve(pool, addr, cli.proxy_token.unwrap_or_default(), cache)
            .await
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = cli;
        anyhow::bail!("--role control requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// `--role all`: the control plane and the proxy in one process, sharing one
/// snapshot in memory. This is what makes `admin_port` writes visible to the
/// very next proxied request with no HTTP round trip back into the same
/// process and no poll delay — `control::api::serve`'s `refresh()` stores
/// into the exact `ArcSwap` `run_data_plane` reads on the request path.
async fn run_all(cli: Cli) -> Result<()> {
    #[cfg(feature = "control")]
    {
        let db_url = cli
            .database_url
            .clone()
            .context("--role all requires --database-url")?;
        let pool = fastllm_proxy::control::db::connect(&db_url).await?;
        let snap = fastllm_proxy::control::build::build_snapshot(&pool).await?;
        let cache = Arc::new(ArcSwap::from_pointee(snap));

        let admin_addr: SocketAddr = format!("{}:{}", cli.host, cli.admin_port)
            .parse()
            .with_context(|| {
                format!("invalid admin bind address {}:{}", cli.host, cli.admin_port)
            })?;
        let admin_pool = pool.clone();
        let admin_cache = Arc::clone(&cache);
        let admin_token = cli.proxy_token.clone().unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) =
                fastllm_proxy::control::api::serve(admin_pool, admin_addr, admin_token, admin_cache)
                    .await
            {
                error!(error = %e, "control plane admin API exited");
            }
        });
        info!(%admin_addr, "control plane admin API listening");

        run_data_plane(cli, Some(cache)).await
    }
    #[cfg(not(feature = "control"))]
    {
        let _ = cli;
        anyhow::bail!("--role all requires the `control` feature; this binary was built with --no-default-features")
    }
}

/// The proxy listener, common to `--role all` and `--role proxy`.
///
/// `shared_snapshot` is `Some` only for `--role all`: the cell the control
/// plane above already owns and keeps current by itself, so no poller is
/// spawned here for that case. `--role proxy` builds and owns its own cell,
/// filled from a config file (`File` mode) or a control plane (`Http` mode),
/// and *does* spawn a poller — that poller, and SIGHUP in `File` mode, are
/// what makes editing keys or grants live rather than requiring a restart.
async fn run_data_plane(cli: Cli, shared_snapshot: Option<Arc<ArcSwap<Snapshot>>>) -> Result<()> {
    let file_config = FileConfig::load(&cli.config)?;
    let tuning = file_config.fastllm.clone();
    let interner = Interner::default();

    let client = Arc::new(upstream::Upstream::new(
        upstream::Config {
            max_idle_per_host: cli.pool_max_idle,
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(5),
        },
        tls_config()?,
    ));

    // Deprecated: a single shared key is exactly what this release replaces,
    // but silently breaking a running deployment is worse than a warning.
    let master_key = cli
        .master_key
        .clone()
        .or_else(|| file_config.general_settings.master_key.clone())
        .filter(|k| !k.is_empty());
    if master_key.is_some() {
        warn!("--master-key is deprecated; define keys under `auth:` or use a control plane");
    }

    // `mode` records which poller (if any) to start once `state` exists —
    // decided here, once, rather than re-inspecting `cli` after the fact.
    enum Mode {
        /// `--role all`: no poller; the admin API keeps `cache` current.
        Shared,
        /// `File`: no control plane. SIGHUP and a poll both apply.
        File,
        /// `Http`: polls a control plane; falls back to its disk cache (and
        /// then to empty) if that control plane is unreachable at startup.
        Http { url: String, token: String },
    }

    let (cell, mode): (Arc<ArcSwap<Snapshot>>, Mode) = if let Some(cache) = shared_snapshot {
        (cache, Mode::Shared)
    } else if let Some(url) = cli.control_url.clone() {
        let token = cli.proxy_token.clone().unwrap_or_default();
        let http_src = HttpSource::new(
            url.clone(),
            token.clone(),
            cli.snapshot_cache.clone(),
            Arc::clone(&client),
        );
        // A proxy that starts with nothing must still start. Refusing to
        // boot would turn a control-plane outage into a crash-loop, which is
        // exactly the failure this architecture exists to prevent.
        let snap = match http_src.fetch(None).await {
            Ok(Some(s)) => s,
            Ok(None) => http_src.load_cached().unwrap_or_default(),
            Err(e) => {
                warn!(error = %e, "control plane unreachable at startup; falling back to the disk cache");
                http_src.load_cached().unwrap_or_default()
            }
        };
        (
            Arc::new(ArcSwap::from_pointee(snap)),
            Mode::Http { url, token },
        )
    } else {
        let file_src =
            WithLegacyMasterKey::new(FileSource::new(cli.config.clone()), master_key.clone());
        let snap = file_src
            .fetch(None)
            .await?
            .context("config produced no snapshot on first load")?;
        (Arc::new(ArcSwap::from_pointee(snap)), Mode::File)
    };

    if cell.load().open {
        warn!("no keys configured; the proxy accepts unauthenticated requests");
    }
    let registry = Registry::build_from_snapshot(&cell.load(), &interner, None)?;
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
        config_path: cli.config.clone(),
        legacy_master_key: master_key,
        snapshot: cell,
        max_body_bytes: cli.max_body_mb.saturating_mul(1024 * 1024),
        max_retries: cli.max_retries,
        upstream_headers_timeout: Duration::from_secs(cli.upstream_timeout),
        unhealthy_after: tuning.unhealthy_after.max(1),
        started: Instant::now(),
        requests_ok: AtomicU64::new(0),
        requests_failed: AtomicU64::new(0),
    });

    health::spawn(
        Arc::clone(&state),
        Duration::from_secs(cli.health_interval.max(1)),
        Duration::from_secs(cli.health_timeout.max(1)),
    );

    match mode {
        Mode::Shared => {
            // The admin API already stores into this exact cell on every
            // write; nothing here would have anything to poll.
        }
        Mode::File => {
            spawn_reload_listener(Arc::clone(&state));
            if cli.config_poll > 0 {
                let source = WithLegacyMasterKey::new(
                    FileSource::new(cli.config.clone()),
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

/// Root certificates for `https://` api_bases.
///
/// The typical deployment is plain HTTP to nodes on a private network, but the
/// config schema accepts `https://` and a hosted or TLS-terminated endpoint is
/// a legitimate backend — accepting the URL and then failing to connect is the
/// worst of both.
///
/// System roots are preferred so an internal CA already trusted by the host
/// works without extra configuration; the bundled Mozilla set is the fallback
/// for minimal containers that ship no root store at all.
fn tls_config() -> Result<rustls::ClientConfig> {
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
