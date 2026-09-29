//! One stalled backend must not stop the health prober for every other one.
//!
//! The production failure this pins: both proxies' health probers were found
//! stopped -- their metrics scrapers still ran, but not one `GET /v1/models`
//! reached any engine -- so a backend that timeouts had ejected on every
//! replica had nothing left that could bring it back, and a 35B stayed down
//! with nothing wrong with it. The sweep awaited each probe's body with no
//! deadline, so one backend that sent headers and then went silent stopped
//! the sweep for good, for all of them, without logging a thing.
//!
//! A real spawned binary against two backends: one that stalls its body
//! forever, and one that answers normally and counts how often it is asked.
//! If the stalled one can still freeze the sweep, the healthy one is probed
//! once and never again.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Answers with complete headers and part of the body it promised, then
/// holds the connection open and says nothing more -- a host that died
/// mid-response, as far as the reader can tell.
async fn spawn_stalling_backend(port: u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind stalling backend");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                          content-length: 1000\r\n\r\n{\"data\":[",
                    )
                    .await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
                drop(sock);
            });
        }
    });
}

/// A normal backend that counts the probes it receives.
async fn spawn_counting_backend(port: u16, probes: Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind counting backend");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let probes = Arc::clone(&probes);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let probes = Arc::clone(&probes);
                        async move {
                            if req.uri().path() == "/v1/models" {
                                probes.fetch_add(1, Ordering::SeqCst);
                            }
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder()
                                    .status(200)
                                    .header("content-type", "application/json")
                                    .body(http_body_util::Full::new(bytes::Bytes::from(
                                        r#"{"object":"list","data":[{"id":"fine","object":"model"}]}"#,
                                    )))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_body_does_not_stop_the_sweep_for_everyone_else() {
    let probes = Arc::new(AtomicUsize::new(0));
    spawn_stalling_backend(14731).await;
    spawn_counting_backend(14732, Arc::clone(&probes)).await;

    let config = "\
model_list:
  - model_name: stalls
    litellm_params: { api_base: http://127.0.0.1:14731/v1 }
  - model_name: fine
    litellm_params: { api_base: http://127.0.0.1:14732/v1 }
";
    let path = std::env::temp_dir().join("probe-liveness-14730.yaml");
    std::fs::write(&path, config).unwrap();
    let _p = Proc(
        Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
            .args([
                "--config",
                path.to_str().unwrap(),
                "--port",
                "14730",
                "--role",
                "proxy",
                "--health-interval",
                "1",
                "--health-timeout",
                "1",
                // Off, so every `/v1/models` the counting backend sees is the
                // prober's and nothing else's.
                "--engine-scrape-interval",
                "0",
            ])
            .spawn()
            .expect("failed to spawn fastllm-proxy"),
    );

    // Long enough for several sweeps even with each one waiting out the
    // stalled probe's one-second deadline.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && probes.load(Ordering::SeqCst) < 4 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let seen = probes.load(Ordering::SeqCst);
    assert!(
        seen >= 4,
        "the healthy backend was probed {seen} time(s) in 8s; a sweep stuck on the stalled \
         backend probes it once and never again"
    );
}
