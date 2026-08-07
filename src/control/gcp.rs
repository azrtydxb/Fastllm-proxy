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
