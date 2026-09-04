//! TLS for the control plane's admin listener.
//!
//! `/snapshot` carries usable upstream backend credentials (see the schema
//! comment on `providers.upstream_api_key`) and `/usage` is gated by the
//! same bearer token, so the design requires TLS "in any deployment where
//! backends have real credentials". This module is what makes that possible:
//! a cert and key turn `control::api::serve` from plain HTTP into HTTPS. When
//! neither is configured, `serve` falls back to plain HTTP and warns loudly
//! at startup instead — a dev deployment with no real credentials is
//! legitimate and must not be forced into generating a cert it does not need.

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::BufReader;
use std::path::Path;
use tokio::net::{TcpListener, TcpStream};

/// Build a server TLS config from a PEM cert chain and a PEM private key.
///
/// Only ever called when both `--tls-cert` and `--tls-key` are given (see
/// `main.rs`); an absent pair means plain HTTP, handled entirely by
/// `control::api::serve`'s `tls: Option<_>` — there is no third state here.
pub fn load_server_config(cert_path: &Path, key_path: &Path) -> Result<rustls::ServerConfig> {
    // Installing the provider is idempotent and harmless if the data-plane
    // client (`main.rs::tls_config`) already did it in the same process
    // (`--role all`); `let _ =` because a second install returning `Err` is
    // exactly that case, not a real failure.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = load_certs(cert_path)
        .with_context(|| format!("reading TLS certificate chain from {}", cert_path.display()))?;
    let key = load_private_key(key_path)
        .with_context(|| format!("reading TLS private key from {}", key_path.display()))?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS server config from --tls-cert/--tls-key")?;
    // Symmetric with the data-plane client's `tls_config`: without an
    // explicit ALPN offer a strict HTTP/1.1-only peer can refuse to guess.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("no valid PEM certificate found")
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)?
        .context("no private key found in PEM file (expected PKCS#8, RSA, or SEC1/EC)")
}

/// Tells apart the one handshake failure that is routine — a peer that opened
/// the TCP connection and closed it again without sending a single byte —
/// from every other kind, which means bytes *were* exchanged and the
/// handshake still failed (bad/expired certificate, a client that refuses
/// every protocol version this server offers, an untrusted CA, ALPN
/// mismatch, and so on).
///
/// This matches on `io::Error::kind()` rather than inspecting the underlying
/// `rustls::Error` (reachable via `e.get_ref().and_then(|e|
/// e.downcast_ref::<rustls::Error>())`) because `tokio_rustls` does not
/// actually give the connect-and-drop case a `rustls::Error` to downcast:
/// the connection dies during the initial record read, before rustls has
/// parsed anything, so `tokio_rustls` reports it as a bare `io::Error` of
/// kind `UnexpectedEof` with no inner `rustls::Error` at all (confirmed by
/// exercising `TlsAcceptor::accept` directly against a dropped socket during
/// development of this function). A protocol-level failure — the cases this
/// function must keep at WARN — always has bytes to fail on and therefore
/// never presents as `UnexpectedEof`; every rustls-side alert (bad cert,
/// version/ALPN mismatch, unknown CA) is wrapped in a different `io::Error`
/// kind (`InvalidData`, in current `rustls`/`tokio-rustls`). So the `io`
/// error kind alone is a reliable — not just convenient — way to split the
/// two cases.
fn is_routine_disconnect(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::UnexpectedEof
}

/// `axum::serve::Listener` over a TCP listener that TLS-terminates every
/// accepted connection before handing it to axum.
///
/// A failed accept or a failed handshake (a client that speaks plain HTTP to
/// the TLS port, say, or the periodic internet background noise every
/// internet-facing port receives) must not end the listener — the loop below
/// logs and keeps accepting, exactly like `axum::serve::Listener`'s blanket
/// `TcpListener` impl does for a bare accept error.
pub struct TlsListener {
    tcp: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, config: rustls::ServerConfig) -> Self {
        Self {
            tcp,
            acceptor: tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)),
        }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.tcp.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "accepting a TLS connection failed");
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(e) => {
                    // Never fatal: one bad handshake (a health-checker or
                    // scanner speaking plain HTTP at an HTTPS port) must not
                    // take the admin API down for every other caller.
                    //
                    // But not every log level here is equal. `deploy/control.yaml`'s
                    // `tcpSocket` probes (soon `httpGet` against `/healthz`, but a
                    // plain TCP connect-and-close is also exactly what an LB health
                    // check, a port scanner, or `kubectl port-forward` teardown looks
                    // like) open the TCP connection and drop it without sending a
                    // single TLS record. `tokio_rustls` reports that as an
                    // `io::Error` of kind `UnexpectedEof` — see
                    // `is_routine_disconnect`'s doc comment for why that kind
                    // specifically is the routine case. Logging that at WARN every
                    // few seconds, forever, is noise that also buries the WARN that
                    // matters: a real misconfiguration (expired/wrong cert, a client
                    // that refuses the offered protocol version, an untrusted CA)
                    // fails differently — with bytes exchanged and a specific TLS
                    // alert — and that case must keep standing out.
                    if is_routine_disconnect(&e) {
                        tracing::debug!(error = %e, %addr, "TLS handshake aborted before any data (routine probe/scan)");
                    } else {
                        tracing::warn!(error = %e, %addr, "TLS handshake failed");
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates a throwaway self-signed cert/key pair on disk via the
    /// `openssl` CLI, for tests that need real PEM files rather than
    /// hand-written ones. Not `pub`: this stays local to the tests that
    /// construct a `TlsListener`/rustls `ServerConfig` end-to-end (this
    /// module's and `control::api`'s).
    ///
    /// Shelling out rather than adding a cert-generation crate (`rcgen` and
    /// similar) for this alone: this project already assumes `openssl` is on
    /// the operator's PATH (`deploy/README.md`'s `openssl rand -hex 32`), and
    /// `rcgen`'s own dependency tree pulls in a `time` release whose declared
    /// MSRV is above this crate's pinned `rust-version = "1.82"` — not worth
    /// a new dependency, in a version resolution this crate does not
    /// control, just to generate a PEM file only tests need.
    pub(crate) fn self_signed_cert_files(
        dir: &Path,
        sans: &[&str],
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let san_ext = format!(
            "subjectAltName={}",
            sans.iter()
                .map(|s| if s.parse::<std::net::IpAddr>().is_ok() {
                    format!("IP:{s}")
                } else {
                    format!("DNS:{s}")
                })
                .collect::<Vec<_>>()
                .join(",")
        );
        let status = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout"])
            .arg(&key_path)
            .arg("-out")
            .arg(&cert_path)
            .args(["-days", "1", "-subj", "/CN=fastllm-test", "-addext"])
            .arg(&san_ext)
            // Without this, OpenSSL's default `req -x509` config marks a
            // self-signed cert `CA:TRUE` (it doubles as its own root by
            // default) — and rustls's webpki verifier then refuses to accept
            // it as a leaf/server certificate at all
            // (`CaUsedAsEndEntity`), since the same cert here plays both
            // roles: it is the trust anchor the client loads *and* the
            // end-entity cert the server presents.
            .args(["-addext", "basicConstraints=critical,CA:FALSE"])
            .status()
            .expect("the `openssl` CLI must be on PATH to run the TLS round-trip tests");
        assert!(
            status.success(),
            "openssl req failed generating a test cert"
        );
        (cert_path, key_path)
    }

    #[test]
    fn a_self_signed_cert_and_key_load_into_a_server_config() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = self_signed_cert_files(dir.path(), &["localhost"]);
        load_server_config(&cert_path, &key_path).expect("valid PEM cert+key must load");
    }

    #[test]
    fn a_missing_cert_file_is_a_readable_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (_, key_path) = self_signed_cert_files(dir.path(), &["localhost"]);
        let err = load_server_config(&dir.path().join("no-such-cert.pem"), &key_path)
            .expect_err("a missing cert file must not load");
        assert!(err.to_string().contains("certificate"));
    }

    /// The end-to-end proof the design asks for: a real TLS handshake and
    /// HTTP round trip between this crate's own client (`upstream::Upstream`,
    /// the same one `HttpSource` polls `/snapshot` through) and a
    /// `TlsListener`-fronted server, where the client trusts the self-signed
    /// cert *only* via an extra CA bundle — never the system root store,
    /// which would never contain a throwaway per-test cert. This is exactly
    /// the private-CA path a cert-manager-issued, in-cluster control-plane
    /// certificate needs; without it a self-signed cert cannot be verified
    /// and the handshake fails closed, not open.
    #[tokio::test]
    async fn a_client_trusting_the_cert_via_an_extra_ca_bundle_completes_a_tls_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = self_signed_cert_files(dir.path(), &["127.0.0.1"]);
        let server_config = load_server_config(&cert_path, &key_path).unwrap();

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let listener = TlsListener::new(tcp, server_config);
        let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // The client side of the exact CA-bundle mechanism `main.rs`'s
        // `load_extra_ca_bundle` implements: parse the PEM file and add every
        // certificate in it to an otherwise-empty root store, so trust comes
        // from the bundle alone.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        let cert_bytes = std::fs::read(&cert_path).unwrap();
        let mut reader = std::io::BufReader::new(cert_bytes.as_slice());
        for cert in rustls_pemfile::certs(&mut reader) {
            roots.add(cert.unwrap()).unwrap();
        }
        let mut client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"http/1.1".to_vec()];

        let client = crate::upstream::Upstream::new(
            crate::upstream::Config {
                max_idle_per_host: 1,
                idle_timeout: std::time::Duration::from_secs(5),
                connect_timeout: std::time::Duration::from_secs(5),
            },
            client_tls,
        );

        let req = hyper::Request::builder()
            .method("GET")
            .uri(format!("https://{addr}/ping"))
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .unwrap();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), client.request(req))
            .await
            .expect("request must not hang")
            .expect("TLS round trip against a cert trusted only via the CA bundle must succeed");
        assert_eq!(resp.status(), hyper::StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(&body[..], b"pong");
    }

    #[test]
    fn a_bare_unexpected_eof_is_the_routine_case() {
        let e = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(is_routine_disconnect(&e));
    }

    #[test]
    fn any_other_error_kind_is_not_routine() {
        for kind in [
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::Other,
        ] {
            let e = std::io::Error::from(kind);
            assert!(
                !is_routine_disconnect(&e),
                "{kind:?} must not be classified as a routine disconnect"
            );
        }
    }

    /// End-to-end proof, not just the classifier in isolation: a client that
    /// opens the TCP connection and drops it before sending anything — the
    /// literal shape of a `tcpSocket`/health-checker probe — must make
    /// `TlsAcceptor::accept` fail with the same error kind
    /// `is_routine_disconnect` treats as routine. If a future `rustls` or
    /// `tokio-rustls` upgrade ever changes how that case is reported, this
    /// test (not just production logs) is what catches it.
    #[tokio::test]
    async fn connect_and_drop_is_classified_as_routine() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = self_signed_cert_files(dir.path(), &["127.0.0.1"]);
        let config = load_server_config(&cert_path, &key_path).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            drop(stream); // connect, then close without sending a byte
        });
        let (stream, _) = tcp.accept().await.unwrap();
        let err = acceptor
            .accept(stream)
            .await
            .expect_err("a dropped socket must not complete a TLS handshake");
        client.await.unwrap();

        assert!(
            is_routine_disconnect(&err),
            "a bare connect-and-drop must classify as routine, got: {err:?}"
        );
    }

    /// The contrasting case: bytes that are *not* a valid TLS record (here,
    /// plaintext HTTP hitting the TLS port) must fail the handshake without
    /// being classified as a routine disconnect — this is the "real problem"
    /// case that must keep logging at WARN.
    #[tokio::test]
    async fn garbage_bytes_are_not_classified_as_routine() {
        use tokio::io::AsyncWriteExt;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = self_signed_cert_files(dir.path(), &["127.0.0.1"]);
        let config = load_server_config(&cert_path, &key_path).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
            drop(stream);
        });
        let (stream, _) = tcp.accept().await.unwrap();
        let err = acceptor
            .accept(stream)
            .await
            .expect_err("plaintext HTTP must not complete a TLS handshake");
        client.await.unwrap();

        assert!(
            !is_routine_disconnect(&err),
            "a real protocol failure must not be classified as routine, got: {err:?}"
        );
    }
}
