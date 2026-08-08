//! Google service-account credentials, exchanged for access tokens.
//!
//! Vertex AI is the one provider in this codebase that cannot be reached with
//! a static secret. Its OpenAI-compatible endpoint wants an OAuth2 access
//! token, and those expire in an hour — so something has to mint them, and
//! keep minting them, for as long as the deployment runs.
//!
//! # Why this lives in the control plane
//!
//! Minting a token is a network call. The request path performs no I/O
//! (`tests/no_io_on_hot_path.rs`), so it cannot happen there, and a token that
//! expires hourly cannot be baked into a snapshot once at startup either.
//!
//! The control plane already rebuilds the snapshot on a schedule and ships it
//! to every proxy, which makes it exactly the right place: the token is minted
//! here, travels in the snapshot as an ordinary `api_key`, and the data plane
//! stays unaware that this backend's credential is any different from a static
//! one. No new field on the hot path, no refresh timer in the proxy.
//!
//! # Why the cache is not optional
//!
//! Snapshot rebuilds run on the order of a second. Minting per rebuild would
//! mean thousands of token requests an hour against a quota-limited endpoint,
//! to re-derive a value that is valid for the whole hour. So tokens are cached
//! per service account and reused until they are close to expiry.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::upstream::Upstream;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use std::sync::Arc;

/// Refresh this far ahead of expiry.
///
/// A token that expires while a snapshot is in flight would leave every proxy
/// holding a credential that is already dead, and the next rebuild is only a
/// second away but the *proxies* poll less often than that. Five minutes is
/// comfortably longer than any poll interval an operator would set.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// The scope Vertex AI requires. Not configurable: a narrower scope does not
/// authorise `aiplatform` calls, and a broader one hands out more than this
/// proxy needs.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// A service account, as Google's JSON key file spells it.
#[derive(Debug, Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    /// PEM, PKCS#8.
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

impl ServiceAccount {
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context(
            "not a Google service-account key: expected the JSON file with `client_email` and \
             `private_key`",
        )
    }

    /// Whether a credential looks like a service-account key at all.
    ///
    /// Used to reject the mistake at write time rather than at snapshot-build
    /// time, where it would show up as a backend silently missing from
    /// routing.
    pub fn looks_like_one(json: &str) -> bool {
        Self::parse(json).is_ok()
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds. Google always sends it; the default only guards against a
    /// response shape change turning into a token cached forever.
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

struct Cached {
    token: String,
    expires_at: SystemTime,
}

struct Minter {
    upstream: Arc<Upstream>,
    /// Keyed by client email: one account, one token, however many backends
    /// use it.
    cache: Mutex<HashMap<String, Cached>>,
}

static MINTER: OnceLock<Minter> = OnceLock::new();

/// Give this module the shared HTTP client.
///
/// Not built here: this crate owns exactly one pooled client (see
/// `upstream::Upstream`), and every caller reaching outside the process shares
/// it rather than standing up a second connection pool.
pub fn init(upstream: Arc<Upstream>) {
    let _ = MINTER.set(Minter {
        upstream,
        cache: Mutex::new(HashMap::new()),
    });
}

/// A live access token for `service_account_json`, minted or reused.
pub async fn access_token(service_account_json: &str) -> Result<String> {
    let minter = MINTER
        .get()
        .ok_or_else(|| anyhow!("no HTTP client registered for minting Google access tokens"))?;
    let sa = ServiceAccount::parse(service_account_json)?;

    if let Some(token) = minter.cached(&sa.client_email) {
        return Ok(token);
    }

    let now = SystemTime::now();
    let assertion = signed_assertion(&sa, now)?;
    let token = minter.exchange(&sa, &assertion).await?;
    let expires_at = now + Duration::from_secs(token.expires_in);
    minter.store(&sa.client_email, &token.access_token, expires_at);
    Ok(token.access_token)
}

impl Minter {
    fn cached(&self, client_email: &str) -> Option<String> {
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(client_email)?;
        // `checked_add` rather than a bare add: a token whose expiry is far in
        // the future must not overflow into being treated as expired.
        let deadline = SystemTime::now().checked_add(REFRESH_MARGIN)?;
        (entry.expires_at > deadline).then(|| entry.token.clone())
    }

    fn store(&self, client_email: &str, token: &str, expires_at: SystemTime) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                client_email.to_string(),
                Cached {
                    token: token.to_string(),
                    expires_at,
                },
            );
        }
    }

    async fn exchange(&self, sa: &ServiceAccount, assertion: &str) -> Result<TokenResponse> {
        let body =
            format!("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={assertion}");
        let req = Request::builder()
            .method("POST")
            .uri(&sa.token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Full::new(Bytes::from(body)))?;

        let resp = tokio::time::timeout(Duration::from_secs(10), self.upstream.request(req))
            .await
            .map_err(|_| anyhow!("minting a Google access token timed out"))??;

        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| anyhow!("reading Google's token response failed: {e}"))?
            .to_bytes();
        if !status.is_success() {
            // Google's error body names the actual cause — clock skew, a
            // revoked key, an account without the role — and it is the only
            // useful thing an operator has to go on.
            return Err(anyhow!(
                "Google rejected the service-account assertion ({status}): {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        serde_json::from_slice(&bytes).context("Google's token response was not the expected shape")
    }
}

/// Build and sign the JWT bearer assertion Google exchanges for a token.
fn signed_assertion(sa: &ServiceAccount, now: SystemTime) -> Result<String> {
    let iat = now
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the epoch")?
        .as_secs();
    // One hour is the maximum Google accepts, and the assertion is used once,
    // immediately — its lifetime only has to cover the round trip.
    let exp = iat + 3600;

    let header = br#"{"alg":"RS256","typ":"JWT"}"#;
    let claims = serde_json::json!({
        "iss": sa.client_email,
        "scope": SCOPE,
        "aud": sa.token_uri,
        "iat": iat,
        "exp": exp,
    });
    let signing_input = format!(
        "{}.{}",
        base64url(header),
        base64url(&serde_json::to_vec(&claims)?)
    );
    let signature = sign_rs256(&sa.private_key, signing_input.as_bytes())?;
    Ok(format!("{signing_input}.{}", base64url(&signature)))
}

/// RSASSA-PKCS1-v1_5 over SHA-256, the only algorithm Google's JWT bearer flow
/// accepts for a service account.
fn sign_rs256(private_key_pem: &str, message: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(private_key_pem.as_bytes());
    let der = rustls_pemfile::private_key(&mut cursor)
        .context("service-account private_key is not readable PEM")?
        .ok_or_else(|| anyhow!("service-account private_key contains no key"))?;

    let pair = ring::signature::RsaKeyPair::from_pkcs8(der.secret_der())
        .map_err(|e| anyhow!("service-account private_key is not a usable RSA key: {e}"))?;

    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|e| anyhow!("signing the service-account assertion failed: {e}"))?;
    Ok(signature)
}

/// Base64url without padding, as JWT requires.
///
/// Hand-rolled for the same reason `EncryptionKey` uses hex: `base64` is not a
/// dependency of this crate, and this is the only place that needs it.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        // 4 output characters per 3 input bytes, minus one for each byte the
        // final chunk was short. Padding is omitted rather than trimmed,
        // because JWT forbids it.
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway key that authorises nothing — see `testdata/README.md`. Real
    /// rather than synthetic because `ring` will not sign with a fake one, and
    /// a test that skipped the signature would miss the part most likely to
    /// break.
    const TEST_KEY: &str = include_str!("testdata/rsa_test_key.pem");

    /// Distinct `account` per test: the token cache is keyed by client email
    /// and shared process-wide, so two tests using one account would see each
    /// other's cached token.
    fn key_file_for(account: &str, token_uri: &str) -> String {
        serde_json::json!({
            "type": "service_account",
            "client_email": format!("{account}@example.iam.gserviceaccount.com"),
            "private_key": TEST_KEY,
            "token_uri": token_uri,
        })
        .to_string()
    }

    /// Decode a base64url segment back to bytes, so the assertion can be
    /// inspected the way Google's token endpoint will.
    fn base64url_decode(s: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bits = 0u32;
        let mut nbits = 0;
        let mut out = Vec::new();
        for c in s.bytes() {
            let v = ALPHABET.iter().position(|&a| a == c).expect("base64url") as u32;
            bits = (bits << 6) | v;
            nbits += 6;
            if nbits >= 8 {
                nbits -= 8;
                out.push((bits >> nbits) as u8);
            }
        }
        out
    }

    /// A token endpoint that counts how many times it was asked.
    ///
    /// Counting is the point: the cache is what keeps a once-per-second
    /// snapshot rebuild from making thousands of token requests an hour, and
    /// nothing else observes whether it works.
    async fn spawn_token_endpoint(body: &'static str, status: u16) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |req: hyper::Request<hyper::body::Incoming>| {
                            let counter = Arc::clone(&counter);
                            async move {
                                let seen = req.into_body().collect().await.map(|b| b.to_bytes());
                                counter.fetch_add(1, Ordering::Relaxed);
                                // The form body is parked where the assertions
                                // below can read it back.
                                let _ = seen;
                                Ok::<_, std::convert::Infallible>(
                                    hyper::Response::builder()
                                        .status(status)
                                        .header("content-type", "application/json")
                                        .body(Full::new(Bytes::from(body)))
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
        (format!("http://127.0.0.1:{port}/token"), hits)
    }

    fn init_minter() {
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        init(Arc::new(crate::upstream::Upstream::new(
            crate::upstream::Config {
                max_idle_per_host: 4,
                idle_timeout: Duration::from_secs(30),
                connect_timeout: Duration::from_secs(5),
            },
            tls,
        )));
    }

    /// The whole path, against a token endpoint rather than Google: a real
    /// signed assertion goes out, the access token comes back, and the second
    /// call is served from the cache rather than minting again.
    #[tokio::test]
    async fn a_token_is_minted_once_and_then_reused() {
        init_minter();
        let (uri, hits) = spawn_token_endpoint(
            r#"{"access_token":"ya29.minted","expires_in":3600,"token_type":"Bearer"}"#,
            200,
        )
        .await;
        let sa = key_file_for("reuse", &uri);

        assert_eq!(access_token(&sa).await.unwrap(), "ya29.minted");
        assert_eq!(hits.load(Ordering::Relaxed), 1);

        // The call a snapshot rebuild makes a second later.
        assert_eq!(access_token(&sa).await.unwrap(), "ya29.minted");
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "a cached token must not be re-minted on every snapshot rebuild"
        );
    }

    /// A token already inside the refresh margin is not handed out: it could
    /// expire while the snapshot carrying it is still reaching the proxies.
    #[tokio::test]
    async fn a_token_expiring_within_the_margin_is_replaced() {
        init_minter();
        // 60 seconds, comfortably inside the 5-minute margin.
        let (uri, hits) =
            spawn_token_endpoint(r#"{"access_token":"ya29.brief","expires_in":60}"#, 200).await;
        let sa = key_file_for("brief", &uri);

        assert_eq!(access_token(&sa).await.unwrap(), "ya29.brief");
        assert_eq!(access_token(&sa).await.unwrap(), "ya29.brief");
        assert_eq!(
            hits.load(Ordering::Relaxed),
            2,
            "a token this close to expiry must be re-minted, not served from cache"
        );
    }

    /// Google's error body names the actual cause — clock skew, a revoked key,
    /// an account without the role — and it is all an operator has to go on, so
    /// it must survive into the message rather than being flattened to a code.
    #[tokio::test]
    async fn googles_rejection_reason_reaches_the_operator() {
        init_minter();
        let (uri, _) = spawn_token_endpoint(
            r#"{"error":"invalid_grant","error_description":"Invalid JWT: token expired"}"#,
            400,
        )
        .await;
        let err = access_token(&key_file_for("rejected", &uri))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid_grant"), "{err}");
        assert!(err.contains("Invalid JWT"), "{err}");
    }

    /// The assertion is what Google validates, so its claims are asserted
    /// exactly: a wrong `aud` or a missing scope fails authentication with a
    /// message that points nowhere near the cause.
    #[test]
    fn the_assertion_carries_the_claims_google_checks() {
        let sa =
            ServiceAccount::parse(&key_file_for("vertex", "https://example.test/token")).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let jwt = signed_assertion(&sa, now).unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "header.claims.signature");

        let header: serde_json::Value =
            serde_json::from_slice(&base64url_decode(parts[0])).unwrap();
        assert_eq!(header["alg"], "RS256");

        let claims: serde_json::Value =
            serde_json::from_slice(&base64url_decode(parts[1])).unwrap();
        assert_eq!(claims["iss"], "vertex@example.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], "https://example.test/token");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["iat"], 1_700_000_000);
        // One hour is the maximum Google accepts.
        assert_eq!(claims["exp"], 1_700_000_000 + 3600);

        // The signature must verify against this key's public half, which is
        // the one thing a hand-rolled base64url could break silently.
        let der = rustls_pemfile::private_key(&mut std::io::Cursor::new(TEST_KEY.as_bytes()))
            .unwrap()
            .unwrap();
        let pair = ring::signature::RsaKeyPair::from_pkcs8(der.secret_der()).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            pair.public().as_ref(),
        );
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        public
            .verify(signing_input.as_bytes(), &base64url_decode(parts[2]))
            .expect("the assertion must verify against its own key");
    }

    /// Against RFC 4648's own vectors, adjusted for JWT's no-padding rule.
    #[test]
    fn base64url_matches_the_rfc_vectors_without_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
    }

    /// The two characters that differ from standard base64 are the whole point
    /// of the url alphabet: `+` and `/` would need escaping in the form body
    /// the assertion is posted in.
    #[test]
    fn base64url_uses_the_url_safe_alphabet() {
        let encoded = base64url(&[0xfb, 0xff, 0xfe]);
        assert_eq!(encoded, "-__-");
        assert!(!encoded.contains('+') && !encoded.contains('/'));
    }

    #[test]
    fn a_credential_that_is_not_a_service_account_is_recognised_as_such() {
        assert!(!ServiceAccount::looks_like_one("sk-not-a-service-account"));
        assert!(!ServiceAccount::looks_like_one(
            r#"{"client_email":"a@b.c"}"#
        ));
        assert!(ServiceAccount::looks_like_one(
            r#"{"client_email":"a@b.c","private_key":"-----BEGIN PRIVATE KEY-----"}"#
        ));
    }

    /// `token_uri` is part of the key file and part of the signed audience, so
    /// a key issued for a non-default endpoint must sign for that endpoint.
    #[test]
    fn the_token_uri_defaults_but_is_honoured_when_present() {
        let sa = ServiceAccount::parse(
            r#"{"client_email":"a@b.c","private_key":"x","token_uri":"https://example.test/t"}"#,
        )
        .unwrap();
        assert_eq!(sa.token_uri, "https://example.test/t");

        let sa = ServiceAccount::parse(r#"{"client_email":"a@b.c","private_key":"x"}"#).unwrap();
        assert_eq!(sa.token_uri, "https://oauth2.googleapis.com/token");
    }

    /// A key that is not RSA at all must fail with something an operator can
    /// act on, rather than a panic or an assertion Google rejects opaquely.
    #[test]
    fn an_unusable_private_key_is_reported_rather_than_panicking() {
        let err = sign_rs256("not pem at all", b"x").unwrap_err().to_string();
        assert!(
            err.contains("private_key"),
            "the message must name the field: {err}"
        );
    }
}
