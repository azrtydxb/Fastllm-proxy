//! Turns database rows into the snapshot the data plane consumes.
//!
//! All the expensive work happens here, once per change, so that the request
//! path is a set lookup: roles are resolved to permissions, permissions to
//! model names, and wildcards expanded against the known model list.

use crate::snapshot::{BackendDef, KeyEntry, ModelDef, Principal, Snapshot};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

/// Resolve `model:invoke` permissions into a concrete set of model names.
///
/// `model/*` short-circuits to `allow_all` rather than materialising every
/// name, so a grant stays correct when a model is added later. Any other verb
/// is an administrative permission and has no place in the request path.
pub fn flatten_grants(
    perms: &[(String, String)],
    all_models: &[String],
) -> (HashSet<String>, bool) {
    let mut set = HashSet::new();
    for (verb, resource) in perms {
        if verb != "model:invoke" {
            continue;
        }
        let Some(pattern) = resource.strip_prefix("model/") else {
            continue;
        };
        if pattern == "*" {
            return (HashSet::new(), true);
        }
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                set.extend(all_models.iter().filter(|m| m.starts_with(prefix)).cloned())
            }
            None => {
                if all_models.iter().any(|m| m == pattern) {
                    set.insert(pattern.to_string());
                }
            }
        }
    }
    (set, false)
}

/// Build a full snapshot from the current database state.
///
/// This runs one permissions query per principal rather than a single join
/// across principals/roles/permissions. That is deliberate, not an
/// oversight: this runs once per change (never on the request path) against
/// tens of principals, so the N+1 shape costs nothing that matters and stays
/// far easier to read than the joined equivalent.
pub async fn build_snapshot(pool: &PgPool) -> anyhow::Result<Snapshot> {
    let model_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM models ORDER BY name")
            .fetch_all(pool)
            .await?;
    let all_names: Vec<String> = model_rows.iter().map(|(_, n)| n.clone()).collect();

    let backend_rows: Vec<(i64, String, String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT model_id, api_base, upstream_model, upstream_api_key FROM model_backends",
    )
    .fetch_all(pool)
    .await?;

    let mut models = Vec::new();
    for (id, name) in &model_rows {
        models.push(ModelDef {
            name: name.clone(),
            backends: backend_rows
                .iter()
                .filter(|(mid, ..)| mid == id)
                .map(|(_, base, upstream, key)| BackendDef {
                    api_base: base.trim_end_matches('/').to_string(),
                    upstream_model: upstream.clone(),
                    api_key: key
                        .as_ref()
                        .map(|k| String::from_utf8_lossy(k).into_owned()),
                })
                .collect(),
        });
    }

    let principal_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM principals WHERE NOT disabled")
            .fetch_all(pool)
            .await?;

    let mut principals = HashMap::new();
    for (id, name) in principal_rows {
        // One query per principal: see the module-level rationale above.
        let perms: Vec<(String, String)> = sqlx::query_as(
            "SELECT p.verb, p.resource FROM permissions p
             JOIN role_permissions rp ON rp.permission_id = p.id
             JOIN principal_roles pr  ON pr.role_id = rp.role_id
             WHERE pr.principal_id = $1",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;
        let (allowed_models, allow_all) = flatten_grants(&perms, &all_names);
        principals.insert(
            id as u64,
            Principal {
                id: id as u64,
                name,
                allowed_models,
                allow_all,
            },
        );
    }

    type KeyRow = (Vec<u8>, i64, Option<chrono::DateTime<chrono::Utc>>, bool);
    let key_rows: Vec<KeyRow> =
        sqlx::query_as("SELECT hash, principal_id, expires_at, disabled FROM api_keys")
            .fetch_all(pool)
            .await?;

    let mut keys = HashMap::new();
    for (hash, principal, expires, disabled) in key_rows {
        let Ok(hash): Result<[u8; 32], _> = hash.try_into() else {
            continue;
        };
        keys.insert(
            hash,
            KeyEntry {
                principal: principal as u64,
                expires_at: expires.map(|d| d.into()),
                disabled,
            },
        );
    }

    // The version is a clock the control plane owns; the proxy only compares it.
    let version: i64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT")
        .fetch_one(pool)
        .await?;

    Ok(Snapshot {
        version: version as u64,
        keys,
        principals,
        models,
        open: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_resource_grants_every_model() {
        let perms = vec![("model:invoke".to_string(), "model/*".to_string())];
        let (set, all) = flatten_grants(&perms, &["a".into(), "b".into()]);
        assert!(all);
        assert!(set.is_empty(), "allow_all makes the set redundant");
    }

    #[test]
    fn a_named_resource_grants_only_that_model() {
        let perms = vec![("model:invoke".to_string(), "model/qwen3".to_string())];
        let (set, all) = flatten_grants(&perms, &["qwen3".into(), "other".into()]);
        assert!(!all);
        assert_eq!(set, ["qwen3".to_string()].into_iter().collect());
    }

    #[test]
    fn a_prefix_wildcard_expands_against_the_known_models() {
        let perms = vec![("model:invoke".to_string(), "model/qwen*".to_string())];
        let (set, all) = flatten_grants(&perms, &["qwen3".into(), "qwen2".into(), "llama".into()]);
        assert!(!all);
        assert_eq!(
            set,
            ["qwen3".to_string(), "qwen2".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn permissions_other_than_model_invoke_are_ignored_here() {
        // Admin permissions are not inference permissions and must never leak
        // into the request path's grant set. The resource is deliberately
        // "model/*" (rather than "*") so that if the verb guard were ever
        // removed, this would match the wildcard branch and set allow_all —
        // making the test fail instead of passing vacuously.
        let perms = vec![("config:write".to_string(), "model/*".to_string())];
        let (set, all) = flatten_grants(&perms, &["a".into()]);
        assert!(!all);
        assert!(set.is_empty());
    }

    #[test]
    fn grants_from_several_roles_union() {
        let perms = vec![
            ("model:invoke".to_string(), "model/a".to_string()),
            ("model:invoke".to_string(), "model/b".to_string()),
        ];
        let (set, _) = flatten_grants(&perms, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(
            set,
            ["a".to_string(), "b".to_string()].into_iter().collect()
        );
    }
}
