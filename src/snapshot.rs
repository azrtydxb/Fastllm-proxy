//! The contract between the control plane and the data plane.
//!
//! Everything expensive is resolved when this is built, not when a request
//! arrives: roles are flattened into `allowed_models`, wildcards expanded, and
//! deny rules applied. The request path asks a `HashSet` and never walks the
//! RBAC graph — that is what keeps authorisation off the latency budget.

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
}
