//! The contract between the control plane and the data plane.
//!
//! Everything expensive is resolved when this is built, not when a request
//! arrives: roles are flattened into `allowed_models`, wildcards expanded, and
//! deny rules applied. The request path asks a `HashSet` and never walks the
//! RBAC graph — that is what keeps authorisation off the latency budget.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

#[cfg(feature = "control")]
use serde::{Deserialize, Serialize};

pub type PrincipalId = u64;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Unknown,
    Expired,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub id: PrincipalId,
    pub name: String,
    /// Already flattened from roles, wildcards and deny rules.
    pub allowed_models: HashSet<String>,
    pub allow_all: bool,
}

impl Principal {
    #[inline]
    pub fn may_invoke(&self, model: &str) -> bool {
        self.allow_all || self.allowed_models.contains(model)
    }
}

#[derive(Debug, Clone)]
pub struct KeyEntry {
    pub principal: PrincipalId,
    pub expires_at: Option<SystemTime>,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct BackendDef {
    pub api_base: String,
    pub upstream_model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub name: String,
    pub backends: Vec<BackendDef>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub version: u64,
    pub keys: HashMap<[u8; 32], KeyEntry>,
    pub principals: HashMap<PrincipalId, Principal>,
    pub models: Vec<ModelDef>,
    /// When true the proxy serves without authenticating, matching today's
    /// behaviour when no master key is configured.
    pub open: bool,
}

/// SHA-256, deliberately not a slow KDF: API keys are high-entropy random
/// values, so stretching buys nothing and would put milliseconds on every
/// request. Passwords are the opposite case and use Argon2id elsewhere.
pub fn hash_key(token: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().into()
}

/// JSON-safe mirror of [`Snapshot`].
///
/// Exists solely because `[u8; 32]` cannot be a serde map key; key hashes are
/// hex strings here and nowhere else. See [`Snapshot::to_wire`].
#[cfg(feature = "control")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireSnapshot {
    pub version: u64,
    pub keys: HashMap<String, WireKeyEntry>,
    pub principals: Vec<WirePrincipal>,
    pub models: Vec<WireModelDef>,
    pub open: bool,
}

#[cfg(feature = "control")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireKeyEntry {
    pub principal: PrincipalId,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled: bool,
}

#[cfg(feature = "control")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePrincipal {
    pub id: PrincipalId,
    pub name: String,
    pub allowed_models: Vec<String>,
    pub allow_all: bool,
}

#[cfg(feature = "control")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireBackendDef {
    pub api_base: String,
    pub upstream_model: String,
    pub api_key: Option<String>,
}

#[cfg(feature = "control")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireModelDef {
    pub name: String,
    pub backends: Vec<WireBackendDef>,
}

impl Snapshot {
    pub fn authenticate(&self, token: &str, now: SystemTime) -> Result<&Principal, AuthError> {
        let entry = self.keys.get(&hash_key(token)).ok_or(AuthError::Unknown)?;
        if entry.disabled {
            return Err(AuthError::Disabled);
        }
        if entry.expires_at.is_some_and(|e| e <= now) {
            return Err(AuthError::Expired);
        }
        self.principals
            .get(&entry.principal)
            .ok_or(AuthError::Unknown)
    }

    /// Synthesise a single allow-all principal from a `--master-key`/
    /// `general_settings.master_key` value.
    ///
    /// This is the compatibility path: a running deployment upgraded in place
    /// still has exactly the one shared key it always had, and it must keep
    /// working — silently breaking it on upgrade would be worse than the
    /// deprecation warning that comes with this call.
    pub fn add_legacy_master_key(&mut self, key: &str) {
        // Id 0 is reserved for the legacy key so it can never collide with a
        // control-plane-assigned principal id, which starts at 1.
        const LEGACY_PRINCIPAL: PrincipalId = 0;
        self.principals.insert(
            LEGACY_PRINCIPAL,
            Principal {
                id: LEGACY_PRINCIPAL,
                name: "legacy-master-key".into(),
                allowed_models: HashSet::new(),
                allow_all: true,
            },
        );
        self.keys.insert(
            hash_key(key),
            KeyEntry {
                principal: LEGACY_PRINCIPAL,
                expires_at: None,
                disabled: false,
            },
        );
        self.open = false;
    }

    /// Convert to the JSON-safe mirror sent over `/snapshot` and written to
    /// the on-disk cache.
    ///
    /// `[u8; 32]` cannot be a serde map key, so key hashes are hex-encoded
    /// here rather than reworking the in-memory representation that the
    /// request path's `HashMap` lookup depends on.
    #[cfg(feature = "control")]
    pub fn to_wire(&self) -> WireSnapshot {
        WireSnapshot {
            version: self.version,
            keys: self
                .keys
                .iter()
                .map(|(hash, entry)| {
                    (
                        hex::encode(hash),
                        WireKeyEntry {
                            principal: entry.principal,
                            expires_at: entry.expires_at.map(chrono::DateTime::<chrono::Utc>::from),
                            disabled: entry.disabled,
                        },
                    )
                })
                .collect(),
            principals: self
                .principals
                .values()
                .map(|p| WirePrincipal {
                    id: p.id,
                    name: p.name.clone(),
                    allowed_models: p.allowed_models.iter().cloned().collect(),
                    allow_all: p.allow_all,
                })
                .collect(),
            models: self
                .models
                .iter()
                .map(|m| WireModelDef {
                    name: m.name.clone(),
                    backends: m
                        .backends
                        .iter()
                        .map(|b| WireBackendDef {
                            api_base: b.api_base.clone(),
                            upstream_model: b.upstream_model.clone(),
                            api_key: b.api_key.clone(),
                        })
                        .collect(),
                })
                .collect(),
            open: self.open,
        }
    }

    /// Inverse of [`Snapshot::to_wire`].
    ///
    /// A hash that does not decode to exactly 32 bytes is skipped rather than
    /// panicking: this is untrusted-ish data crossing a process boundary
    /// (control plane -> disk or network -> proxy), and one malformed entry
    /// must not take the whole snapshot down.
    #[cfg(feature = "control")]
    pub fn from_wire(w: WireSnapshot) -> Snapshot {
        let mut keys = HashMap::new();
        for (hex_hash, entry) in w.keys {
            let Ok(bytes) = hex::decode(&hex_hash) else {
                continue;
            };
            let Ok(hash): Result<[u8; 32], _> = bytes.try_into() else {
                continue;
            };
            keys.insert(
                hash,
                KeyEntry {
                    principal: entry.principal,
                    expires_at: entry.expires_at.map(SystemTime::from),
                    disabled: entry.disabled,
                },
            );
        }
        Snapshot {
            version: w.version,
            keys,
            principals: w
                .principals
                .into_iter()
                .map(|p| {
                    (
                        p.id,
                        Principal {
                            id: p.id,
                            name: p.name,
                            allowed_models: p.allowed_models.into_iter().collect(),
                            allow_all: p.allow_all,
                        },
                    )
                })
                .collect(),
            models: w
                .models
                .into_iter()
                .map(|m| ModelDef {
                    name: m.name,
                    backends: m
                        .backends
                        .into_iter()
                        .map(|b| BackendDef {
                            api_base: b.api_base,
                            upstream_model: b.upstream_model,
                            api_key: b.api_key,
                        })
                        .collect(),
                })
                .collect(),
            open: w.open,
        }
    }

    #[cfg(test)]
    pub fn for_test(
        keys: Vec<(String, PrincipalId, Option<SystemTime>, bool)>,
        principals: Vec<Principal>,
        models: Vec<ModelDef>,
    ) -> Self {
        Self {
            version: 1,
            keys: keys
                .into_iter()
                .map(|(k, p, e, d)| {
                    (
                        hash_key(&k),
                        KeyEntry {
                            principal: p,
                            expires_at: e,
                            disabled: d,
                        },
                    )
                })
                .collect(),
            principals: principals.into_iter().map(|p| (p.id, p)).collect(),
            models,
            open: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn snapshot_with(key: &str, allowed: &[&str], expires: Option<SystemTime>) -> Snapshot {
        let principal = Principal {
            id: 1,
            name: "eval-team".into(),
            allowed_models: allowed.iter().map(|s| s.to_string()).collect(),
            allow_all: false,
        };
        Snapshot::for_test(
            vec![(key.to_string(), 1, expires, false)],
            vec![principal],
            vec![],
        )
    }

    #[test]
    fn a_known_key_resolves_to_its_principal() {
        let snap = snapshot_with("sk-good", &["m"], None);
        let p = snap.authenticate("sk-good", SystemTime::now()).unwrap();
        assert_eq!(p.name, "eval-team");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let snap = snapshot_with("sk-good", &["m"], None);
        assert!(matches!(
            snap.authenticate("sk-bad", SystemTime::now()),
            Err(AuthError::Unknown)
        ));
    }

    #[test]
    fn an_expired_key_is_rejected_even_though_it_is_known() {
        let past = SystemTime::now() - Duration::from_secs(60);
        let snap = snapshot_with("sk-good", &["m"], Some(past));
        assert!(matches!(
            snap.authenticate("sk-good", SystemTime::now()),
            Err(AuthError::Expired)
        ));
    }

    #[test]
    fn model_grants_are_exact_unless_allow_all() {
        let snap = snapshot_with("sk-good", &["qwen3", "whisper"], None);
        let p = snap.authenticate("sk-good", SystemTime::now()).unwrap();
        assert!(p.may_invoke("qwen3"));
        assert!(p.may_invoke("whisper"));
        assert!(!p.may_invoke("gpt-4"));
    }

    #[test]
    fn allow_all_grants_every_model() {
        let mut snap = snapshot_with("sk-admin", &[], None);
        snap.principals.get_mut(&1).unwrap().allow_all = true;
        let p = snap.authenticate("sk-admin", SystemTime::now()).unwrap();
        assert!(p.may_invoke("anything-at-all"));
    }

    #[test]
    fn hashing_is_stable_and_distinct() {
        assert_eq!(hash_key("sk-a"), hash_key("sk-a"));
        assert_ne!(hash_key("sk-a"), hash_key("sk-b"));
    }

    /// Load-bearing for Task 7: the on-disk cache stores exactly this JSON,
    /// so anything lost here is lost on every cold start after a control
    /// plane outage.
    #[cfg(feature = "control")]
    #[test]
    fn a_snapshot_survives_a_round_trip_through_the_wire_format() {
        let expiry = SystemTime::now() + Duration::from_secs(3600);
        let mut snap = snapshot_with("sk-good", &["qwen3", "whisper"], Some(expiry));
        snap.version = 42;
        snap.principals.get_mut(&1).unwrap().allow_all = false;
        snap.models.push(ModelDef {
            name: "qwen3".into(),
            backends: vec![crate::snapshot::BackendDef {
                api_base: "http://node-a:8000".into(),
                upstream_model: "qwen3-upstream".into(),
                api_key: Some("upstream-secret".into()),
            }],
        });

        let json = serde_json::to_string(&snap.to_wire()).unwrap();
        let wire: WireSnapshot = serde_json::from_str(&json).unwrap();
        let round_tripped = Snapshot::from_wire(wire);

        assert_eq!(round_tripped.version, 42);
        assert_eq!(round_tripped.open, snap.open);

        let key_hash = hash_key("sk-good");
        let original_entry = &snap.keys[&key_hash];
        let restored_entry = &round_tripped.keys[&key_hash];
        assert_eq!(restored_entry.principal, original_entry.principal);
        assert_eq!(restored_entry.disabled, original_entry.disabled);
        // SystemTime -> chrono -> SystemTime loses sub-second precision on
        // some platforms; compare to the second instead of bit-for-bit.
        let original_secs = original_entry
            .expires_at
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let restored_secs = restored_entry
            .expires_at
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(restored_secs, original_secs);

        let original_principal = &snap.principals[&1];
        let restored_principal = &round_tripped.principals[&1];
        assert_eq!(restored_principal.name, original_principal.name);
        assert_eq!(restored_principal.allow_all, original_principal.allow_all);
        assert_eq!(
            restored_principal.allowed_models,
            original_principal.allowed_models
        );

        assert_eq!(round_tripped.models.len(), 1);
        let model = &round_tripped.models[0];
        assert_eq!(model.name, "qwen3");
        assert_eq!(model.backends.len(), 1);
        assert_eq!(model.backends[0].api_base, "http://node-a:8000");
        assert_eq!(model.backends[0].upstream_model, "qwen3-upstream");
        assert_eq!(
            model.backends[0].api_key.as_deref(),
            Some("upstream-secret")
        );
    }
}
