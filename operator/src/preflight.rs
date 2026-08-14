//! Resolving — and judging — the Secrets before anything is deployed.
//!
//! # Why the controller reads Secrets at all
//!
//! Two jobs, and neither can be done by pointing a `secretKeyRef` at them.
//!
//! **Saying what is wrong.** A missing Secret, a typo in a key name or a
//! 31-byte encryption key all produce the same thing without this: pods stuck
//! in `CreateContainerConfigError` or a control plane crash-looping on
//! startup, and a `FastllmProxy` whose status says only that no replica is
//! ready. The cause is one `kubectl describe` away and the operator has to
//! know to look. Resolving them here turns every one of those into a
//! condition that names the Secret, the key and the reason.
//!
//! **Rotation.** Env from a `secretKeyRef` is resolved once, when the
//! container starts. Rewriting the Secret afterwards changes nothing until
//! something happens to restart the pod — so a rotated proxy token or a
//! cert-manager renewal looks applied and is not. Hashing the resolved bytes
//! into the pod template makes the rotation itself the rollout.
//!
//! # What it does not do
//!
//! Log, echo, or store any of the material. The hash is one-way and the
//! plaintext never leaves this module.

use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use sha2::{Digest, Sha256};
use std::fmt;

use crate::crd::{FastllmProxySpec, SecretRef};

/// AES-256: 32 bytes, so 64 hex characters. Mirrors
/// `fastllm_proxy::control::secrets::EncryptionKey::from_hex`, which is what
/// the control plane will do with it at startup — the point of checking here
/// is to fail before a rollout rather than after one.
const ENCRYPTION_KEY_BYTES: usize = 32;

#[derive(Debug, PartialEq)]
pub enum Invalid {
    MissingSecret {
        secret: String,
    },
    MissingKey {
        secret: String,
        key: String,
    },
    /// The value is present and unusable. Carries *why*, never the value.
    BadValue {
        secret: String,
        key: String,
        why: String,
    },
}

impl Invalid {
    /// The `reason` of the condition this becomes — a short CamelCase token,
    /// as Kubernetes conditions want.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::MissingSecret { .. } => "SecretNotFound",
            Self::MissingKey { .. } => "KeyNotFound",
            Self::BadValue { .. } => "InvalidValue",
        }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSecret { secret } => {
                write!(f, "Secret {secret:?} does not exist in this namespace")
            }
            Self::MissingKey { secret, key } => {
                write!(f, "Secret {secret:?} has no key {key:?}")
            }
            Self::BadValue { secret, key, why } => {
                write!(f, "{secret:?} key {key:?}: {why}")
            }
        }
    }
}

/// What a successful preflight produces: a hash and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// Hex SHA-256 over every resolved value plus the tuning file, truncated
    /// to 16 characters. Long enough that a collision is not a practical
    /// concern for a handful of inputs, short enough to read in
    /// `kubectl get -o yaml` — it is an annotation an operator compares by
    /// eye, not a signature.
    pub config_hash: String,
}

/// Read one key out of one Secret.
async fn value(api: &Api<Secret>, r: &SecretRef) -> Result<Vec<u8>, Invalid> {
    let secret = api
        .get_opt(&r.name)
        .await
        .map_err(|_| Invalid::MissingSecret {
            secret: r.name.clone(),
        })?
        .ok_or_else(|| Invalid::MissingSecret {
            secret: r.name.clone(),
        })?;

    // `data` is base64 in the wire format and decoded by the client;
    // `stringData` is write-only and never comes back, so this is the only
    // field to read.
    secret
        .data
        .as_ref()
        .and_then(|d| d.get(&r.key))
        .map(|b| b.0.clone())
        .ok_or_else(|| Invalid::MissingKey {
            secret: r.name.clone(),
            key: r.key.clone(),
        })
}

fn bad(r: &SecretRef, why: impl Into<String>) -> Invalid {
    Invalid::BadValue {
        secret: r.name.clone(),
        key: r.key.clone(),
        why: why.into(),
    }
}

/// Resolve every Secret the deployment needs, validate what can be validated
/// cheaply, and hash the result.
///
/// Errors are returned one at a time, in the order an operator would fix
/// them: there is no value in reporting a bad encryption key while the
/// database Secret does not exist yet.
pub async fn resolve(
    api: &Api<Secret>,
    spec: &FastllmProxySpec,
    tuning: &str,
) -> Result<Resolved, Invalid> {
    let mut hasher = Sha256::new();

    let database = value(api, &spec.database).await?;
    let url = std::str::from_utf8(&database).map_err(|_| bad(&spec.database, "not valid UTF-8"))?;
    let url = url.trim();
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        // Checked because the alternative is a control plane that starts,
        // fails to connect, and reports it as a database outage.
        return Err(bad(
            &spec.database,
            "does not look like a Postgres URL (expected a postgres:// or postgresql:// scheme)",
        ));
    }
    hasher.update(b"database\0");
    hasher.update(url.as_bytes());

    let token = value(api, &spec.proxy_token).await?;
    if token.iter().all(|b| b.is_ascii_whitespace()) {
        // An empty token authenticates nothing, and both planes would accept
        // it from each other without ever saying so.
        return Err(bad(&spec.proxy_token, "is empty"));
    }
    hasher.update(b"proxy_token\0");
    hasher.update(&token);

    let key = value(api, &spec.encryption_key).await?;
    let key =
        std::str::from_utf8(&key).map_err(|_| bad(&spec.encryption_key, "not valid UTF-8"))?;
    let decoded = hex::decode(key.trim()).map_err(|_| {
        bad(
            &spec.encryption_key,
            "is not valid hex; generate one with `openssl rand -hex 32`",
        )
    })?;
    if decoded.len() != ENCRYPTION_KEY_BYTES {
        return Err(bad(
            &spec.encryption_key,
            format!(
                "decodes to {} bytes, not {ENCRYPTION_KEY_BYTES} — AES-256 needs exactly \
                 {ENCRYPTION_KEY_BYTES}, so the control plane would refuse to start",
                decoded.len()
            ),
        ));
    }
    hasher.update(b"encryption_key\0");
    hasher.update(&decoded);

    if let Some(b) = &spec.bootstrap {
        let password = value(api, &b.password).await?;
        if password.is_empty() {
            return Err(bad(&b.password, "is empty"));
        }
        // Deliberately *not* hashed in: the admin password is not something
        // the running pods read, and rotating it should not roll the gateway.
        // The bootstrap Job reads it, once.
    }

    // The TLS material is mounted as a volume rather than read as env, so the
    // kubelet does update the files in place on renewal — but the control
    // plane reads its certificate once, at startup, and would go on serving
    // the expired one. Hashing it makes a renewal a rollout.
    if let Some(name) = &spec.control.tls_secret_name {
        for key in ["tls.crt", "tls.key", "ca.crt"] {
            let r = SecretRef {
                name: name.clone(),
                key: key.to_string(),
            };
            let bytes = value(api, &r).await?;
            hasher.update(key.as_bytes());
            hasher.update(b"\0");
            hasher.update(&bytes);
        }
    }

    hasher.update(b"tuning\0");
    hasher.update(tuning.as_bytes());

    let digest = hasher.finalize();
    Ok(Resolved {
        config_hash: hex::encode(&digest[..8]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_carries_a_reason_kubernetes_can_group_by() {
        assert_eq!(
            Invalid::MissingSecret { secret: "s".into() }.reason(),
            "SecretNotFound"
        );
        assert_eq!(
            Invalid::MissingKey {
                secret: "s".into(),
                key: "k".into()
            }
            .reason(),
            "KeyNotFound"
        );
    }

    /// The whole point of the message is that it names what to fix. A
    /// condition saying "invalid configuration" would be no better than the
    /// `CreateContainerConfigError` this exists to replace.
    #[test]
    fn the_message_names_the_secret_and_the_key() {
        let m = Invalid::MissingKey {
            secret: "fastllm-secrets".into(),
            key: "proxy-token".into(),
        }
        .to_string();
        assert!(m.contains("fastllm-secrets"), "{m}");
        assert!(m.contains("proxy-token"), "{m}");
    }

    /// A rejected key must never be echoed back — a status subresource is
    /// world-readable to anyone who can `get` the CR, which is a much wider
    /// set than "can read the Secret".
    #[test]
    fn a_bad_value_is_never_quoted_in_the_message() {
        let secret_material = "deadbeef";
        let m = bad(
            &SecretRef {
                name: "s".into(),
                key: "encryption-key".into(),
            },
            "decodes to 4 bytes, not 32",
        )
        .to_string();
        assert!(!m.contains(secret_material), "{m}");
    }
}
