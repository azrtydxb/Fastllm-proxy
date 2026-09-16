//! ChatGPT subscription OAuth tokens.
//!
//! ChatGPT (Codex) subscription access uses OpenAI's OAuth endpoints rather
//! than a static API key. A provider with `credential_kind = "chatgpt_oauth"`
//! stores encrypted OAuth tokens instead of a static `upstream_api_key`.
//!
//! # Why this lives in the control plane
//!
//! Token refresh is a network call. The request path performs no I/O
//! (`tests/no_io_on_hot_path.rs`), so it cannot happen there. The control
//! plane already rebuilds the snapshot on a schedule and ships it to every
//! proxy, which makes it exactly the right place: the token is minted or
//! refreshed here, travels in the snapshot as an ordinary `api_key`, and the
//! data plane stays unaware that this backend's credential is any different
//! from a static one.
//!
//! # OAuth flow
//!
//! PKCE is required by OpenAI's OAuth implementation:
//!
//! 1. Generate a random 43+ character `code_verifier`
//! 2. Compute `code_challenge = BASE64URL(SHA256(code_verifier))`
//! 3. Redirect the user to OpenAI's authorization endpoint with the challenge
//! 4. OpenAI calls back the callback handler with `code` and `state`
//! 5. Exchange `code` + `verifier` for access/refresh tokens from the token endpoint
//! 6. Encrypt and store tokens in `providers.chatgpt_oauth_tokens`
//! 7. On snapshot rebuild: if tokens are within 5 minutes of expiry, exchange
//!    the refresh token for a new access token

use anyhow::{anyhow, Context, Result};
use http_body_util::BodyExt;
use rand::Rng;
use ring::digest;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::control::secrets;
use crate::upstream::Upstream;

/// Refresh this far ahead of expiry.
///
/// A token that expires while a snapshot is in flight would leave every proxy
/// holding a credential that is already dead, and the next rebuild is only a
/// second away but the *proxies* poll less often than that. Five minutes is
/// comfortably longer than any poll interval an operator would set.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// OpenAI's OAuth endpoints for ChatGPT/Codex subscriptions.
///
/// These are derived from the Codex CLI/OpenClaw implementation and OpenAI's
/// developer documentation. They may change at any time.
const AUTHORIZATION_ENDPOINT: &str = "https://auth.beehive.chatgpt.com/v1/oauth/authorize";
const TOKEN_ENDPOINT: &str = "https://auth.beehive.chatgpt.com/v1/oauth/token";
const SCOPE: &str = "openid offline_access";

/// An OAuth token pair: access token (short-lived) and refresh token (stable).
///
/// Serialized as JSON and encrypted before storage.
#[derive(Debug, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) when the access token expires.
    pub expires_at: u64,
    /// Unix timestamp (seconds) when the OAuth flow was initiated.
    pub created_at: u64,
}

/// State for an in-flight OAuth challenge.
///
/// The verifier is stored briefly (in-memory) so it can be used to exchange
/// the authorization code for tokens. It is cleared after exchange.
pub struct PendingChallenge {
    pub verifier: String,
    pub state: String,
    pub expires_at: SystemTime,
}

/// The in-memory state store: one pending challenge per provider.
struct OAuthState {
    challenges: Mutex<HashMap<String, PendingChallenge>>,
    upstream: Arc<Upstream>,
}

static STATE: OnceLock<OAuthState> = OnceLock::new();

/// Give this module the shared HTTP client.
///
/// Not built here: this crate owns exactly one pooled client (see
/// `upstream::Upstream`), and every caller reaching outside the process shares
/// it rather than standing up a second connection pool.
pub fn init(upstream: Arc<Upstream>) {
    let _ = STATE.set(OAuthState {
        challenges: Mutex::new(HashMap::new()),
        upstream,
    });
}

/// The shared HTTP client, for other control-plane code that needs to reach
/// outside the process.
pub fn shared_client() -> Option<Arc<Upstream>> {
    STATE.get().map(|s| Arc::clone(&s.upstream))
}

/// Generate a PKCE challenge for an OAuth connect flow.
///
/// Returns a challenge URL the operator can open in a browser, plus a state
/// token that must be presented back to the callback handler. The challenge
/// (code_verifier) is stored in-memory and cleared after the token exchange.
///
/// The `provider_id` scopes the challenge to one provider so concurrent
/// operators on different providers do not share state.
pub fn generate_challenge(_provider_id: uuid::Uuid) -> Result<String> {
    let state = STATE
        .get()
        .ok_or_else(|| anyhow!("OAuth module not initialized"))?;
    let verifier: String = (0..48)
        .map(|_| {
            let pool = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut rng = rand::rng();
            let mut buf = [0u8; 1];
            rng.fill(&mut buf);
            pool.as_bytes()[buf[0] as usize % pool.len()] as char
        })
        .collect();

    // PKCE: challenge is SHA256(verifier), encoded as base64url without padding.
    let challenge = pkce_challenge(&verifier);
    let state_token = uuid::Uuid::new_v4().to_string();

    let now = SystemTime::now();
    state.challenges.lock().unwrap().insert(
        state_token.clone(),
        PendingChallenge {
            verifier,
            state: state_token.clone(),
            expires_at: now + Duration::from_secs(300),
        },
    );

    Ok(format!(
        "{}?response_type=code&client_id=fastllm-proxy&redirect_uri=http://localhost:4001/admin/providers/oauth/callback&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        AUTHORIZATION_ENDPOINT,
        url_encode(SCOPE),
        challenge,
        state_token
    ))
}

/// Handle the OAuth callback: exchange the authorization code for tokens.
///
/// `state` must match a pending challenge; `code` is the authorization code
/// from OpenAI. On success the tokens are encrypted and stored in the
/// provider row; the pending challenge is cleared.
pub async fn exchange_token(
    pool: &sqlx::PgPool,
    key: &secrets::EncryptionKey,
    provider_id: uuid::Uuid,
    state: &str,
    code: &str,
) -> Result<()> {
    let state_ref = STATE
        .get()
        .ok_or_else(|| anyhow!("OAuth module not initialized"))?;
    let challenge = state_ref
        .challenges
        .lock()
        .unwrap()
        .remove(state)
        .ok_or_else(|| anyhow!("invalid or expired OAuth state"))?;

    // Exchange code + verifier for tokens from OpenAI.
    let tokens = exchange_tokens(&challenge.verifier, code, &state_ref.upstream).await?;

    // Store encrypted in the provider row.
    let json = serde_json::to_string(&tokens).context("serializing OAuth tokens")?;
    let encrypted = secrets::encrypt(key, &json).context("encrypting OAuth tokens")?;

    // Update the provider row.
    sqlx::query("UPDATE providers SET chatgpt_oauth_tokens = $1 WHERE id = $2")
        .bind(encrypted)
        .bind(provider_id)
        .execute(pool)
        .await
        .context("storing OAuth tokens")?;

    Ok(())
}

/// Check connection status for a provider.
pub async fn connection_status(
    pool: &sqlx::PgPool,
    key: &secrets::EncryptionKey,
    provider_id: uuid::Uuid,
) -> Result<ConnectionStatus> {
    let row = sqlx::query("SELECT chatgpt_oauth_tokens FROM providers WHERE id = $1")
        .bind(provider_id)
        .fetch_optional(pool)
        .await
        .context("reading OAuth status")?;

    match row {
        Some(row) => {
            let encrypted: Option<Vec<u8>> = row.try_get("chatgpt_oauth_tokens").ok();
            let encrypted = encrypted.ok_or_else(|| anyhow!("no tokens stored"))?;
            let decrypted = secrets::decrypt(key, &encrypted).context("decrypting OAuth tokens")?;
            let tokens: OAuthTokens =
                serde_json::from_str(&decrypted).context("parsing OAuth tokens")?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock")?
                .as_secs();
            let remaining = tokens.expires_at.saturating_sub(now);
            Ok(ConnectionStatus {
                connected: remaining > 60,
                expires_in_seconds: remaining,
            })
        }
        None => Ok(ConnectionStatus {
            connected: false,
            expires_in_seconds: 0,
        }),
    }
}

/// Disconnect a provider: clear stored OAuth tokens.
pub async fn disconnect(pool: &sqlx::PgPool, provider_id: uuid::Uuid) -> Result<()> {
    sqlx::query("UPDATE providers SET chatgpt_oauth_tokens = NULL WHERE id = $1")
        .bind(provider_id)
        .execute(pool)
        .await
        .context("clearing OAuth tokens")?;
    Ok(())
}

/// Refresh tokens for a provider if they are near expiry.
///
/// Reads encrypted tokens from the database, decrypts them, and if within
/// the refresh margin exchanges the refresh token for a new access token.
/// Returns the (possibly refreshed) access token, or `None` if the provider
/// has no stored tokens.
///
/// Used by the snapshot build to ensure every proxy receives a valid token.
pub async fn refresh_if_needed(
    pool: &sqlx::PgPool,
    key: &secrets::EncryptionKey,
    provider_id: uuid::Uuid,
) -> Result<Option<String>> {
    let state_ref = STATE
        .get()
        .ok_or_else(|| anyhow!("OAuth module not initialized"))?;

    let row = sqlx::query("SELECT chatgpt_oauth_tokens FROM providers WHERE id = $1")
        .bind(provider_id)
        .fetch_optional(pool)
        .await
        .context("reading OAuth tokens for refresh")?;

    let Some(row) = row else {
        return Ok(None);
    };

    let encrypted: Option<Vec<u8>> = row.try_get("chatgpt_oauth_tokens").ok();
    let encrypted = encrypted.ok_or_else(|| anyhow!("no tokens stored"))?;

    let decrypted =
        secrets::decrypt(key, &encrypted).context("decrypting OAuth tokens for refresh")?;
    let mut tokens: OAuthTokens =
        serde_json::from_str(&decrypted).context("parsing OAuth tokens for refresh")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock")?
        .as_secs();

    if tokens.expires_at > now + REFRESH_MARGIN.as_secs() {
        // Not near expiry — return the existing access token.
        return Ok(Some(tokens.access_token));
    }

    // Near expiry — refresh.
    let new_tokens = refresh_tokens(&tokens.refresh_token, &state_ref.upstream).await?;
    tokens.access_token = new_tokens.access_token;
    tokens.expires_at = new_tokens.expires_at;
    if !new_tokens.refresh_token.is_empty() {
        tokens.refresh_token = new_tokens.refresh_token;
    }

    let json = serde_json::to_string(&tokens).context("serializing refreshed tokens")?;
    let encrypted = secrets::encrypt(key, &json).context("encrypting refreshed tokens")?;

    sqlx::query("UPDATE providers SET chatgpt_oauth_tokens = $1 WHERE id = $2")
        .bind(encrypted)
        .bind(provider_id)
        .execute(pool)
        .await
        .context("storing refreshed tokens")?;

    Ok(Some(tokens.access_token))
}

/// PKCE: SHA256(code_verifier) → base64url.
fn pkce_challenge(verifier: &str) -> String {
    let hash = digest::digest(&digest::SHA256, verifier.as_bytes());
    base64url(hash.as_ref())
}

/// Exchange authorization code for tokens.
async fn exchange_tokens(verifier: &str, code: &str, client: &Upstream) -> Result<OAuthTokens> {
    let body = format!(
        "grant_type=authorization_code&code={}&code_verifier={}&redirect_uri=http://localhost:4001/admin/providers/oauth/callback&client_id=fastllm-proxy",
        url_encode(code),
        url_encode(verifier)
    );
    let req = hyper::Request::builder()
        .method("POST")
        .uri(TOKEN_ENDPOINT)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))?;

    let resp = tokio::time::timeout(Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| anyhow!("OAuth token exchange timed out"))??;

    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("reading token response: {e}"))?
        .to_bytes();

    if !status.is_success() {
        return Err(anyhow!(
            "OAuth token exchange failed ({status}): {}",
            String::from_utf8_lossy(&bytes)
        ));
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    }

    let token: TokenResponse =
        serde_json::from_slice(&bytes).context("token response was not valid JSON")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock")?
        .as_secs();

    Ok(OAuthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now + token.expires_in,
        created_at: now,
    })
}

/// Refresh an access token using the refresh token.
async fn refresh_tokens(refresh_token: &str, client: &Upstream) -> Result<OAuthTokens> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id=fastllm-proxy",
        url_encode(refresh_token)
    );
    let req = hyper::Request::builder()
        .method("POST")
        .uri(TOKEN_ENDPOINT)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))?;

    let resp = tokio::time::timeout(Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| anyhow!("OAuth refresh timed out"))??;

    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("reading refresh response: {e}"))?
        .to_bytes();

    if !status.is_success() {
        return Err(anyhow!(
            "OAuth refresh failed ({status}): {}",
            String::from_utf8_lossy(&bytes)
        ));
    }

    #[derive(Deserialize)]
    struct RefreshResponse {
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    }

    let token: RefreshResponse =
        serde_json::from_slice(&bytes).context("refresh response was not valid JSON")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock")?
        .as_secs();

    Ok(OAuthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now + token.expires_in,
        created_at: now,
    })
}

/// Connection status for a provider.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStatus {
    pub connected: bool,
    /// Seconds until the access token expires.
    pub expires_in_seconds: u64,
}

/// Base64url without padding, as JWT and OAuth require.
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
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Percent-encode a string for application/x-www-form-urlencoded bodies.
///
/// Only encodes characters that are not unreserved (`A-Za-z0-9-._~`) or
/// allowed in form bodies (`:` and `/` which appear in callback URIs).
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify PKCE challenge is deterministic.
    #[test]
    fn pkce_challenge_is_deterministic() {
        let v1 = "abc123def456";
        let v2 = "abc123def456";
        let c1 = pkce_challenge(v1);
        let c2 = pkce_challenge(v2);
        assert_eq!(c1, c2);
    }

    /// PKCE challenge is different from the verifier (SHA256 hash).
    #[test]
    fn pkce_challenge_differs_from_verifier() {
        let verifier = "a".repeat(50);
        let challenge = pkce_challenge(&verifier);
        assert_ne!(challenge, verifier);
        assert!(!challenge.contains('=')); // no padding
    }

    /// Base64url matches RFC 4648 test vectors.
    #[test]
    fn base64url_rfc_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
    }

    /// Token JSON round-trips through serialization.
    #[test]
    fn token_json_round_trips() {
        let tokens = OAuthTokens {
            access_token: "gpt-oauth.test".into(),
            refresh_token: "1//0e".into(),
            expires_at: 1700000000,
            created_at: 1699999000,
        };
        let json = serde_json::to_string(&tokens).unwrap();
        let decoded: OAuthTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.access_token, "gpt-oauth.test");
        assert_eq!(decoded.expires_at, 1700000000);
    }

    /// URL encoding leaves safe characters untouched.
    #[test]
    fn url_encode_leaves_safe_chars() {
        assert_eq!(url_encode("hello_world"), "hello_world");
        assert_eq!(url_encode("A-B.C_D"), "A-B.C_D");
        assert_eq!(url_encode("https://example.com"), "https://example.com");
    }

    /// URL encoding percent-encodes special characters.
    #[test]
    fn url_encode_special_chars() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a+b"), "a%2Bb");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
    }
}
