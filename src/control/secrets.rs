//! Encryption at rest for `providers.upstream_api_key`.
//!
//! This protects the database, not the snapshot: `/snapshot` still carries
//! the credential in usable plaintext form, because the proxy has to
//! present it to the backend as a bearer token — an upstream credential
//! cannot be reduced to a hash the way a user-facing API key is
//! (`api_keys.hash`). What this buys is narrower and still worth having:
//! someone with `SELECT` on Postgres (a backup, a read replica, a leaked
//! pg_dump, a careless `GRANT`) no longer walks away with every upstream
//! bearer token for free. It buys nothing against someone who can read
//! `/snapshot` or the running process's memory — see `migrations/0002` and
//! `migrations/0004` for the schema-comment history of this claim.
//!
//! ## Format
//!
//! `version_byte || nonce (12 bytes) || ciphertext_and_tag`. The version
//! byte is checked on every decrypt and a blob whose byte we don't
//! recognise is rejected outright rather than guessed at — that is what
//! lets a future key-rotation or algorithm change add a new version byte
//! without ambiguity against rows written under an earlier one.
//!
//! Two versions currently exist. `1` binds the version byte to nothing —
//! AES-GCM's authenticated-data input is empty (`Aad::empty()`) — so the one
//! byte that selects which code path decrypts a blob is itself unauthenticated
//! and attacker-malleable: nothing before `2` stopped a byte for byte
//! flip of just that leading byte on a `1` blob from being handed to a
//! different decrypt path (unexploitable while `1` is the only version that
//! exists, but not a property to leave standing once a second version does).
//! `2` fixes that by passing the version byte itself as AEAD associated data
//! (`Aad::from([VERSION_V2])`), so tampering with it now fails the
//! authentication check the same way tampering with the ciphertext already
//! did. [`encrypt`] only ever writes `2`; [`decrypt`] still reads `1` (with
//! empty AAD, exactly as it always did) so every already-encrypted row in a
//! live database keeps decrypting without a re-encryption pass — the two
//! version bytes select two different AAD inputs to the same tag check, not
//! two different ciphertexts to migrate between.
//!
//! AES-256-GCM via `ring::aead`, which is already in the dependency tree
//! (rustls pulls it in for TLS) — this module promotes it to a direct
//! dependency rather than adding a new crypto crate. The nonce is 96 bits of
//! `ring::rand::SystemRandom` output, freshly drawn per call to `encrypt`;
//! at the volume this table sees (backends configured by an operator, not a
//! per-request hot path) random-nonce collision risk is not a practical
//! concern.
//!
//! ## Key handling
//!
//! [`EncryptionKey::from_env`] reads `FASTLLM_ENCRYPTION_KEY` as hex (not
//! base64 — chosen only because `hex` is already a direct dependency of
//! this crate and base64 is not, so hex adds nothing new to decode it with;
//! either encoding would have been fine). It must decode to exactly 32
//! bytes. Anything else — unset, malformed hex, wrong length — is a hard
//! startup failure. There is deliberately no fallback path that stores a
//! credential in plaintext because the key was missing; see
//! `src/main.rs`'s control-plane startup for where this is enforced.
//!
//! Never log a key or a decrypted credential. `EncryptionKey` intentionally
//! has no `Display` and a `Debug` impl that prints nothing but the type
//! name, so an accidental `{:?}` in a log line cannot leak key material.

use anyhow::{bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};

/// Raw key length required by AES-256-GCM.
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// The original format: `Aad::empty()`. Still decrypted (never written by
/// [`encrypt`] anymore) so existing rows keep working — see the module doc
/// comment.
const VERSION_V1: u8 = 1;

/// Current format: identical to `V1` except the version byte itself is
/// authenticated as AEAD associated data (`Aad::from([VERSION_V2])`) rather
/// than left outside the tag. [`encrypt`] always writes this version now.
const VERSION_V2: u8 = 2;

/// A loaded 32-byte AES-256-GCM key. Holds the raw bytes only long enough to
/// construct a `ring` key object; never printed, never serialized.
#[derive(Clone)]
pub struct EncryptionKey {
    raw: [u8; KEY_LEN],
}

impl std::fmt::Debug for EncryptionKey {
    /// Deliberately omits the key material — see the module doc comment on
    /// never logging key bytes. `{:?}` on this type is safe to leave in a
    /// log statement by accident.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncryptionKey(..)")
    }
}

impl EncryptionKey {
    /// Load the 32-byte key from `FASTLLM_ENCRYPTION_KEY`, hex-encoded.
    ///
    /// Fails loudly — missing var, bad hex, wrong decoded length are all
    /// hard errors — rather than falling back to "no encryption" for any of
    /// them. Callers on the control-plane startup path (`--role
    /// control`/`all`, and the `import`/`reencrypt-backends` commands) are
    /// expected to propagate this error and refuse to start, not catch it.
    pub fn from_env() -> Result<Self> {
        let val = std::env::var("FASTLLM_ENCRYPTION_KEY").context(
            "FASTLLM_ENCRYPTION_KEY is required: the control plane stores upstream backend \
             credentials encrypted at rest and refuses to start without a key rather than \
             silently falling back to storing them in plaintext. Generate one with \
             `openssl rand -hex 32`.",
        )?;
        Self::from_hex(&val)
    }

    /// Parse a hex-encoded 32-byte key directly. Split out from
    /// [`from_env`] so tests can construct a key without touching process
    /// environment (which is global, mutable, shared-per-process state —
    /// exactly what parallel `#[test]` runs must not race on).
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s.trim()).context("FASTLLM_ENCRYPTION_KEY is not valid hex")?;
        if bytes.len() != KEY_LEN {
            bail!(
                "FASTLLM_ENCRYPTION_KEY must decode to exactly {KEY_LEN} bytes, got {}",
                bytes.len()
            );
        }
        let mut raw = [0u8; KEY_LEN];
        raw.copy_from_slice(&bytes);
        Ok(Self { raw })
    }

    fn less_safe_key(&self) -> Result<LessSafeKey> {
        let unbound = UnboundKey::new(&AES_256_GCM, &self.raw)
            .map_err(|_| anyhow::anyhow!("failed to construct AES-256-GCM key"))?;
        Ok(LessSafeKey::new(unbound))
    }
}

/// Encrypt `plaintext` under `key`, returning `version || nonce || ciphertext+tag`.
///
/// Always writes [`VERSION_V2`] — the version byte is bound in as AEAD
/// associated data, so it is authenticated the same way the ciphertext is;
/// see the module doc comment. There is no code path left that writes `V1`.
///
/// A fresh random nonce is drawn for every call — never reuse a nonce
/// across two `encrypt` calls under the same key, which is why the nonce is
/// generated here rather than accepted as a parameter.
pub fn encrypt(key: &EncryptionKey, plaintext: &str) -> Result<Vec<u8>> {
    let less_safe = key.less_safe_key()?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate a random nonce"))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plaintext.as_bytes().to_vec();
    less_safe
        .seal_in_place_append_tag(nonce, Aad::from([VERSION_V2]), &mut in_out)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    let mut out = Vec::with_capacity(1 + NONCE_LEN + in_out.len());
    out.push(VERSION_V2);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&in_out);
    Ok(out)
}

/// Decrypt a blob produced by [`encrypt`]. Rejects (does not guess at):
/// an empty or truncated blob, an unrecognised version byte, and — via
/// AES-GCM's authentication tag — any blob that was tampered with or
/// encrypted under a different key.
///
/// Understands both versions: `1` (empty AAD — every row written before
/// `VERSION_V2` existed) and `2` (the version byte bound in as AAD, current
/// and the only version [`encrypt`] writes). Which AAD is used is picked
/// from the version byte and is *not* itself a guess protected by anything
/// — under `2` a flipped version byte fails the tag check same as flipped
/// ciphertext would; under `1` it never carried that protection and still
/// does not, which is the whole reason `1` is being retired for new writes
/// rather than kept indefinitely.
pub fn decrypt(key: &EncryptionKey, blob: &[u8]) -> Result<String> {
    let Some(&version) = blob.first() else {
        bail!("empty ciphertext blob");
    };
    let aad = match version {
        VERSION_V1 => Aad::from(&[][..]),
        VERSION_V2 => Aad::from(&[VERSION_V2][..]),
        other => bail!(
            "unrecognised encryption format version byte {other}; this build only understands \
             versions {VERSION_V1} and {VERSION_V2}"
        ),
    };
    if blob.len() < 1 + NONCE_LEN {
        bail!("ciphertext blob too short to contain a nonce");
    }

    let nonce_bytes: [u8; NONCE_LEN] = blob[1..1 + NONCE_LEN]
        .try_into()
        .expect("length checked above");
    let mut in_out = blob[1 + NONCE_LEN..].to_vec();

    let less_safe = key.less_safe_key()?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let plaintext = less_safe
        .open_in_place(nonce, aad, &mut in_out)
        .map_err(|_| {
            anyhow::anyhow!(
                "decryption failed: authentication check failed (tampered ciphertext, wrong \
                 key, or corrupt data)"
            )
        })?;

    String::from_utf8(plaintext.to_vec()).context("decrypted plaintext was not valid UTF-8")
}

/// True if `blob` decrypts successfully under `key`.
///
/// Used only by the one-shot `reencrypt-backends` maintenance command
/// (`src/control/import.rs`) to tell an already-migrated row (a valid
/// `encrypt`-produced blob) from a pre-migration plaintext row (raw
/// credential bytes, as `import` wrote them before this module existed).
/// AES-GCM's authentication tag makes a false positive here — plaintext
/// bytes that happen to look like a valid version byte, nonce and
/// authenticated ciphertext under this exact key — astronomically unlikely,
/// which is what makes "did it decrypt" a safe classifier rather than a
/// guess.
pub fn is_encrypted(key: &EncryptionKey, blob: &[u8]) -> bool {
    decrypt(key, blob).is_ok()
}

/// A fixed key shared by every `#[cfg(test)]` in `control::*` that touches
/// the scratch Postgres database (`control::import::tests`,
/// `control::api::tests`). Those tests share one database across the whole
/// `cargo test` invocation, and `build_snapshot`/`rebuild_once` decrypt
/// *every* non-null `upstream_api_key` row they see, not just the ones a
/// given test wrote — a per-module key made `build_snapshot` in one
/// module's test fail to decrypt a row a different module's test had left
/// behind. One key, used everywhere in these tests, is what makes that
/// safe regardless of what other tests ran first.
#[cfg(test)]
pub(crate) fn test_key() -> EncryptionKey {
    EncryptionKey::from_hex(&"ff".repeat(KEY_LEN)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> EncryptionKey {
        EncryptionKey::from_hex(&"11".repeat(KEY_LEN)).unwrap()
    }

    fn other_key() -> EncryptionKey {
        EncryptionKey::from_hex(&"22".repeat(KEY_LEN)).unwrap()
    }

    #[test]
    fn round_trips_a_credential() {
        let key = test_key();
        let blob = encrypt(&key, "sk-super-secret-upstream-token").unwrap();
        // Ciphertext must not contain the plaintext credential anywhere.
        assert!(!blob.windows(3).any(|w| w == b"sk-"));
        let recovered = decrypt(&key, &blob).unwrap();
        assert_eq!(recovered, "sk-super-secret-upstream-token");
    }

    #[test]
    fn rejects_a_tampered_ciphertext() {
        let key = test_key();
        let mut blob = encrypt(&key, "sk-super-secret-upstream-token").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01; // flip a bit inside the GCM tag
        let err = decrypt(&key, &blob).unwrap_err();
        assert!(err.to_string().contains("authentication"));
    }

    #[test]
    fn rejects_decryption_under_the_wrong_key() {
        let key = test_key();
        let blob = encrypt(&key, "sk-super-secret-upstream-token").unwrap();
        let err = decrypt(&other_key(), &blob).unwrap_err();
        assert!(err.to_string().contains("authentication"));
    }

    /// Builds a blob in the pre-fix `V1` shape directly (`Aad::empty()`,
    /// leading byte `VERSION_V1`), the same way every row [`encrypt`] wrote
    /// before this fix existed. [`encrypt`] itself only ever produces `V2`
    /// blobs now, so a test proving `V1` blobs already in a database still
    /// decrypt has to construct one by hand rather than through `encrypt`.
    fn legacy_v1_blob(key: &EncryptionKey, plaintext: &str) -> Vec<u8> {
        let less_safe = key.less_safe_key().unwrap();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce_bytes).unwrap();
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.as_bytes().to_vec();
        less_safe
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .unwrap();
        let mut out = Vec::with_capacity(1 + NONCE_LEN + in_out.len());
        out.push(VERSION_V1);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&in_out);
        out
    }

    /// Regression for the review finding: nothing may silently break rows
    /// already in the database. Every row written before this fix is a `V1`
    /// blob under empty AAD — those must keep decrypting exactly as before,
    /// with no re-encryption pass required.
    #[test]
    fn a_legacy_v1_blob_still_decrypts() {
        let key = test_key();
        let blob = legacy_v1_blob(&key, "sk-legacy-row-written-before-the-fix");
        assert_eq!(blob[0], VERSION_V1);
        assert_eq!(
            decrypt(&key, &blob).unwrap(),
            "sk-legacy-row-written-before-the-fix"
        );
    }

    /// `encrypt` must not still be writing the format the review finding
    /// flagged.
    #[test]
    fn encrypt_writes_v2_not_v1() {
        let key = test_key();
        let blob = encrypt(&key, "sk-whatever").unwrap();
        assert_eq!(blob[0], VERSION_V2);
    }

    /// The actual fix, proven directly: under `V2`, the version byte is
    /// bound into the AEAD tag as associated data, so flipping it — the
    /// exact malleability the review finding described, "the byte that
    /// selects the code path is attacker-malleable" — now fails
    /// authentication instead of silently being reinterpreted. Contrast
    /// with `V1` (`Aad::empty()`), where the version byte was never
    /// authenticated at all and this same flip only fails today because
    /// `1` is the only version `V1`'s decrypt path recognises — a
    /// distinction that stops holding the moment a second `V1`-shaped
    /// version could ever exist, which is exactly why `V2` exists instead of
    /// leaving `V1` as the permanent format.
    #[test]
    fn the_v2_version_byte_is_authenticated_not_just_checked() {
        let key = test_key();
        let mut blob = encrypt(&key, "sk-super-secret-upstream-token").unwrap();
        assert_eq!(blob[0], VERSION_V2);
        // Flip it to the *other* version byte this build understands, not
        // to noise: this is the case that matters, because it is the one a
        // pre-fix implementation (version byte outside the tag) could not
        // tell apart from a legitimate `V1` blob at all.
        blob[0] = VERSION_V1;
        let err = decrypt(&key, &blob).unwrap_err();
        assert!(
            err.to_string().contains("authentication"),
            "a tampered version byte on a V2 blob must fail the AEAD tag check: {err}"
        );
    }

    #[test]
    fn rejects_an_unknown_version_byte() {
        let key = test_key();
        let mut blob = encrypt(&key, "sk-super-secret-upstream-token").unwrap();
        blob[0] = 0xFF;
        let err = decrypt(&key, &blob).unwrap_err();
        assert!(err.to_string().contains("unrecognised"));
    }

    #[test]
    fn rejects_an_empty_blob() {
        let key = test_key();
        assert!(decrypt(&key, &[]).is_err());
    }

    #[test]
    fn rejects_a_truncated_blob() {
        let key = test_key();
        // Version byte plus a nonce too short to be real.
        assert!(decrypt(&key, &[VERSION_V1, 1, 2, 3]).is_err());
    }

    #[test]
    fn two_encryptions_of_the_same_plaintext_differ() {
        // Random per-call nonce: ciphertext must not be deterministic.
        let key = test_key();
        let a = encrypt(&key, "same-secret").unwrap();
        let b = encrypt(&key, "same-secret").unwrap();
        assert_ne!(a, b);
        assert_eq!(decrypt(&key, &a).unwrap(), "same-secret");
        assert_eq!(decrypt(&key, &b).unwrap(), "same-secret");
    }

    #[test]
    fn is_encrypted_distinguishes_ciphertext_from_plaintext_bytes() {
        let key = test_key();
        let blob = encrypt(&key, "sk-real-token").unwrap();
        assert!(is_encrypted(&key, &blob));
        // Raw plaintext bytes, as a pre-migration row would have stored them.
        assert!(!is_encrypted(&key, b"sk-real-token"));
    }

    #[test]
    fn from_env_rejects_missing_key() {
        // Explicit removal (not relying on ambient state) so this test is
        // correct however the surrounding suite happened to set the var.
        std::env::remove_var("FASTLLM_ENCRYPTION_KEY");
        assert!(EncryptionKey::from_env().is_err());
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        assert!(EncryptionKey::from_hex("abcd").is_err());
    }

    #[test]
    fn from_hex_rejects_non_hex() {
        assert!(EncryptionKey::from_hex(&"zz".repeat(KEY_LEN)).is_err());
    }
}
