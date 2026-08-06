//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

mod config;
mod health;
mod proxy;
mod registry;
mod router;
mod state;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::config::FileConfig;
use crate::registry::{Interner, Registry};
use crate::router::{Policy, Router};
use crate::state::AppState;

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

    #[arg(long, default_value = "info", env = "FASTLLM_LOG")]
    log: String,
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
    let file_config = FileConfig::load(&cli.config)?;
    let tuning = file_config.fastllm.clone();

    let interner = Interner::default();
    let registry = Registry::build(&file_config, &interner, None)?;
    if registry.backends().is_empty() {
        warn!("config lists no backends; every request will 404 until it is reloaded");
    }
    info!(
        models = registry.model_names().len(),
        backends = registry.backends().len(),
        policy = ?cli.policy,
        "loaded {}",
        cli.config.display()
    );

    // TCP_NODELAY matters on both hops: without it a small SSE frame can sit in
    // the kernel waiting for a co-tenant, adding tens of milliseconds of
    // inter-token jitter that looks like slow generation.
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    connector.set_keepalive(Some(Duration::from_secs(60)));

    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(cli.pool_max_idle)
        .retry_canceled_requests(false)
        .build(connector);

    let master_key = cli
        .master_key
        .or_else(|| file_config.general_settings.master_key.clone())
        .filter(|k| !k.is_empty());
    if master_key.is_none() {
        warn!("no master key configured; the proxy accepts unauthenticated requests");
    }

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
        master_key,
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
    spawn_reload_listener(Arc::clone(&state));

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

/// SIGHUP reloads the config in place.
///
/// This is the whole point of owning the process: the model set changes when a
/// workload is launched or stopped, and applying that should not mean tearing
/// down a gateway that has generations in flight.
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
            match state.reload() {
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
