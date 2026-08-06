//! The contract between the control plane and the data plane.
//!
//! Everything expensive is resolved when this is built, not when a request
//! arrives: roles are flattened into `allowed_models`, wildcards expanded, and
//! deny rules applied. The request path asks a `HashSet` and never walks the
//! RBAC graph — that is what keeps authorisation off the latency budget.

use crate::routing::{CallerMatch, RoutingRule, ShapeMatch, VirtualModelDef, WeightedTarget};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

pub type PrincipalId = u64;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Unknown,
    Expired,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: PrincipalId,
    pub name: String,
    /// Already flattened from roles, wildcards and deny rules.
    pub allowed_models: HashSet<String>,
    pub allow_all: bool,
    /// Role *names* this principal holds, for the "caller" match condition on
    /// a routing rule (`crate::routing::CallerMatch`). Unlike
    /// `allowed_models`, this is not further flattened — a rule asks "does
    /// this principal hold role X", never "what can role X invoke" (that
    /// question is already answered by `allowed_models`), so the raw role
    /// name set is exactly what the request path needs.
    pub roles: HashSet<String>,
}

impl Principal {
    #[inline]
    pub fn may_invoke(&self, model: &str) -> bool {
        self.allow_all || self.allowed_models.contains(model)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    pub principal: PrincipalId,
    pub expires_at: Option<SystemTime>,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDef {
    pub api_base: String,
    pub upstream_model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Virtual models, keyed by their client-facing name for an O(1) lookup
    /// on the request path before falling back to treating the requested
    /// name as a concrete model (`proxy::resolve_model`). Empty in `File`
    /// mode: virtual models are a control-plane-only feature (P1 depends on
    /// P0's database), and a bare YAML config has nowhere to store rules.
    pub virtual_models: HashMap<String, VirtualModelDef>,
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

/// Comparison that does not short-circuit on the first differing byte, so a
/// wrong secret cannot be recovered one byte at a time via response timing.
/// Used for the proxy's own bootstrap token (the sole gate on `/snapshot`,
/// which discloses every key hash and usable upstream backend credentials),
/// not for user API keys — those go through `hash_key` and a `HashMap`
/// lookup, which is already constant-time in the relevant sense.
///
/// Do not simplify this back to `==`: length is compared without branching
/// on it either, so neither the length nor any byte position leaks through
/// timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_ok = (a.len() == b.len()) as u8;
    let byte_diff = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    len_ok & ((byte_diff == 0) as u8) == 1
}

/// JSON-safe mirror of [`Snapshot`].
///
/// Exists solely because `[u8; 32]` cannot be a serde map key; key hashes are
/// hex strings here and nowhere else. See [`Snapshot::to_wire`].
///
/// Unconditional: this is the contract between the control plane and the data
/// plane, not control-plane-only code. `HttpSource` (`src/source/http.rs`)
/// needs it in every build, including `--no-default-features`, which has no
/// database driver at all but still has to poll `/snapshot` and cache the
/// result to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireSnapshot {
    pub version: u64,
    pub keys: HashMap<String, WireKeyEntry>,
    pub principals: Vec<WirePrincipal>,
    pub models: Vec<WireModelDef>,
    /// Absent from a snapshot cached on disk before this field existed —
    /// `#[serde(default)]` is what lets an `Http`-mode proxy read an
    /// old last-known-good cache written by a previous version rather than
    /// failing to deserialise it outright.
    #[serde(default)]
    pub virtual_models: Vec<WireVirtualModel>,
    pub open: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireKeyEntry {
    pub principal: PrincipalId,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePrincipal {
    pub id: PrincipalId,
    pub name: String,
    pub allowed_models: Vec<String>,
    pub allow_all: bool,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireBackendDef {
    pub api_base: String,
    pub upstream_model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireModelDef {
    pub name: String,
    pub backends: Vec<WireBackendDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireWeightedTarget {
    pub model: String,
    pub weight: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireCallerMatch {
    #[serde(default)]
    pub principals: Vec<PrincipalId>,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireShapeMatch {
    #[serde(default)]
    pub min_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub max_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub min_max_tokens: Option<u64>,
    #[serde(default)]
    pub max_max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireRoutingRule {
    #[serde(default)]
    pub caller: WireCallerMatch,
    #[serde(default)]
    pub shape: WireShapeMatch,
    pub targets: Vec<WireWeightedTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireVirtualModel {
    pub name: String,
    pub rules: Vec<WireRoutingRule>,
    pub default_targets: Vec<WireWeightedTarget>,
}

impl Snapshot {
    /// Whether two snapshots carry the same policy, ignoring `version`.
    ///
    /// `version` is the database's clock (`EXTRACT(EPOCH FROM now())`), not a
    /// content hash — it advances on every rebuild regardless of whether any
    /// row actually changed. A periodic rebuilder that published on every
    /// tick would therefore always look "changed" and defeat the point of
    /// comparing at all (every tick would rebuild the routing `Registry` and
    /// spam an info log). This is what lets the rebuilder tell "the database
    /// was polled" apart from "the database actually changed".
    pub fn content_eq(&self, other: &Snapshot) -> bool {
        self.keys == other.keys
            && self.principals == other.principals
            && self.models == other.models
            && self.virtual_models == other.virtual_models
            && self.open == other.open
    }

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
                roles: HashSet::new(),
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
                    roles: p.roles.iter().cloned().collect(),
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
            virtual_models: self
                .virtual_models
                .values()
                .map(|vm| WireVirtualModel {
                    name: vm.name.clone(),
                    rules: vm
                        .rules
                        .iter()
                        .map(|r| WireRoutingRule {
                            caller: WireCallerMatch {
                                principals: r.caller.principals.iter().copied().collect(),
                                roles: r.caller.roles.iter().cloned().collect(),
                            },
                            shape: WireShapeMatch {
                                min_prompt_tokens: r.shape.min_prompt_tokens,
                                max_prompt_tokens: r.shape.max_prompt_tokens,
                                min_max_tokens: r.shape.min_max_tokens,
                                max_max_tokens: r.shape.max_max_tokens,
                            },
                            targets: r
                                .targets
                                .iter()
                                .map(|t| WireWeightedTarget {
                                    model: t.model.clone(),
                                    weight: t.weight,
                                })
                                .collect(),
                        })
                        .collect(),
                    default_targets: vm
                        .default_targets
                        .iter()
                        .map(|t| WireWeightedTarget {
                            model: t.model.clone(),
                            weight: t.weight,
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
                            roles: p.roles.into_iter().collect(),
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
            virtual_models: w
                .virtual_models
                .into_iter()
                .map(|vm| {
                    (
                        vm.name.clone(),
                        VirtualModelDef {
                            name: vm.name,
                            rules: vm
                                .rules
                                .into_iter()
                                .map(|r| RoutingRule {
                                    caller: CallerMatch {
                                        principals: r.caller.principals.into_iter().collect(),
                                        roles: r.caller.roles.into_iter().collect(),
                                    },
                                    shape: ShapeMatch {
                                        min_prompt_tokens: r.shape.min_prompt_tokens,
                                        max_prompt_tokens: r.shape.max_prompt_tokens,
                                        min_max_tokens: r.shape.min_max_tokens,
                                        max_max_tokens: r.shape.max_max_tokens,
                                    },
                                    targets: r
                                        .targets
                                        .into_iter()
                                        .map(|t| WeightedTarget {
                                            model: t.model,
                                            weight: t.weight,
                                        })
                                        .collect(),
                                })
                                .collect(),
                            default_targets: vm
                                .default_targets
                                .into_iter()
                                .map(|t| WeightedTarget {
                                    model: t.model,
                                    weight: t.weight,
                                })
                                .collect(),
                        },
                    )
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
            virtual_models: HashMap::new(),
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
            roles: HashSet::new(),
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

    /// `version` is a timestamp, not a content hash — the periodic control
    /// plane rebuilder (`control::api::rebuild_once`) relies on `content_eq`
    /// to tell "the database was polled" apart from "the database actually
    /// changed" without that clock forcing every tick to look like a change.
    #[test]
    fn content_eq_ignores_version_but_not_policy() {
        let mut a = snapshot_with("sk-good", &["qwen3"], None);
        a.version = 1;
        let mut b = snapshot_with("sk-good", &["qwen3"], None);
        b.version = 999;
        assert!(a.content_eq(&b), "only the timestamp differs");

        let mut c = snapshot_with("sk-good", &["qwen3", "llama"], None);
        c.version = 1;
        assert!(
            !a.content_eq(&c),
            "a real grant change must not compare equal"
        );
    }

    #[test]
    fn hashing_is_stable_and_distinct() {
        assert_eq!(hash_key("sk-a"), hash_key("sk-a"));
        assert_ne!(hash_key("sk-a"), hash_key("sk-b"));
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    /// Load-bearing for Task 7: the on-disk cache stores exactly this JSON,
    /// so anything lost here is lost on every cold start after a control
    /// plane outage.
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

    /// Task 7 feeds `from_wire` whatever is on disk, and a truncated or
    /// otherwise corrupted cache file is an expected failure mode there, not
    /// a bug — it must degrade to "missing entry", never panic.
    #[test]
    fn from_wire_skips_corrupt_hashes_instead_of_panicking() {
        let valid_hash = hash_key("sk-good");
        let mut keys = HashMap::new();
        keys.insert(
            "not-hex-at-all!!".to_string(),
            WireKeyEntry {
                principal: 1,
                expires_at: None,
                disabled: false,
            },
        );
        keys.insert(
            hex::encode([0u8; 4]), // valid hex, far short of 32 bytes
            WireKeyEntry {
                principal: 1,
                expires_at: None,
                disabled: false,
            },
        );
        keys.insert(
            hex::encode(valid_hash),
            WireKeyEntry {
                principal: 1,
                expires_at: None,
                disabled: false,
            },
        );

        let wire = WireSnapshot {
            version: 1,
            keys,
            principals: vec![],
            models: vec![],
            virtual_models: vec![],
            open: false,
        };

        let snap = Snapshot::from_wire(wire);

        assert_eq!(snap.keys.len(), 1, "only the valid entry should survive");
        assert!(snap.keys.contains_key(&valid_hash));
    }
}
