//! Turns database rows into the snapshot the data plane consumes.
//!
//! All the expensive work happens here, once per change, so that the request
//! path is a set lookup: roles are resolved to permissions, permissions to
//! model names, and wildcards expanded against the known model list.

use crate::control::secrets::{self, EncryptionKey};
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
///
/// `key` decrypts `model_backends.upstream_api_key` (see `control::secrets`
/// for the format and exactly what encryption at rest here does and does
/// not protect). The snapshot this returns still carries the credential in
/// usable plaintext form — the proxy has to present it to the backend — so
/// this decrypts the database's copy, it does not add protection to the
/// snapshot itself. `/snapshot` must be TLS wherever a backend has a real
/// credential, same as before this module existed.
pub async fn build_snapshot(pool: &PgPool, key: &EncryptionKey) -> anyhow::Result<Snapshot> {
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
        let mut backends = Vec::new();
        for (_, base, upstream, encrypted_key) in backend_rows.iter().filter(|(mid, ..)| mid == id)
        {
            // A decrypt failure here is contained to this one backend, not
            // propagated as a `build_snapshot` error. It used to be: one row
            // that failed to decrypt (a partially completed key rotation, a
            // row `reencrypt_plaintext_backends` never reached, anything not
            // matching `key`) failed the *entire* snapshot — a hard exit on
            // the startup path (`main.rs`'s `EncryptionKey::from_env`
            // sibling calls all propagate `?`), crash-looping the whole
            // control plane and taking every model offline over one bad row.
            // Dropping just the affected backend and logging it loudly is
            // fail-closed on the credential that actually cannot be used,
            // without failing closed on every model that has nothing to do
            // with it. If this was a model's only backend, that model ends
            // up with none, which is exactly the "no healthy backend" case
            // the proxy already has to handle for other reasons (every
            // backend down, none configured).
            let api_key = match encrypted_key.as_ref() {
                None => None,
                Some(blob) => match secrets::decrypt(key, blob) {
                    Ok(plaintext) => Some(plaintext),
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            model = %name,
                            api_base = %base,
                            "dropping backend: upstream_api_key failed to decrypt; excluded from \
                             this snapshot rather than failing the whole rebuild. Re-encrypt it \
                             with the current FASTLLM_ENCRYPTION_KEY (see \
                             `reencrypt-backends`/`is_encrypted`) or delete and recreate it \
                             through the admin API."
                        );
                        continue;
                    }
                },
            };
            backends.push(BackendDef {
                api_base: base.trim_end_matches('/').to_string(),
                upstream_model: upstream.clone(),
                api_key,
            });
        }
        models.push(ModelDef {
            name: name.clone(),
            backends,
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

    fn unique_name(tag: &str) -> String {
        format!(
            "{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Regression for the review finding: one row that fails to decrypt (a
    /// partially completed key rotation, a row never migrated) used to fail
    /// `build_snapshot` outright — a hard exit on the startup path,
    /// crash-looping the control plane and taking every model offline over
    /// one bad backend. It must instead drop just that backend and let the
    /// snapshot build carry every other backend, including a sibling backend
    /// on the very same model.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn one_undecryptable_backend_is_dropped_and_the_rest_of_the_snapshot_still_builds() {
        use crate::control::secrets::test_key;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = test_key();

        let broken_model = unique_name("undecryptable-model");
        let model_id: i64 =
            sqlx::query_scalar("INSERT INTO models (name) VALUES ($1) RETURNING id")
                .bind(&broken_model)
                .fetch_one(&pool)
                .await
                .unwrap();

        // A backend whose `upstream_api_key` cannot possibly decrypt: not
        // `encrypt`-produced ciphertext at all, so `secrets::decrypt` fails
        // on the version byte, the same shape a partially completed key
        // rotation or an unmigrated pre-encryption row would take. Valid
        // UTF-8 (unlike, say, `0xFF` bytes) deliberately: this row is never
        // cleaned up — this whole module's tests share one scratch
        // database with `control::import`'s, including its
        // `reencrypt_migrates_a_plaintext_row_and_is_idempotent`, which
        // scans *every* `model_backends` row with a non-null
        // `upstream_api_key` and treats "does not decrypt" as "must be a
        // pre-migration plaintext row" (`reencrypt_plaintext_backends`'s
        // documented contract) — bytes that are not valid UTF-8 would trip
        // that function's own refuse-to-guess `bail!` on a row this test
        // left behind, in a run where both tests share the table.
        sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             VALUES ($1, 'http://broken:8000/v1', 'broken', $2)",
        )
        .bind(model_id)
        .bind(b"not-an-encrypt-produced-ciphertext-blob-at-all".to_vec())
        .execute(&pool)
        .await
        .unwrap();

        // A healthy sibling backend on the *same* model, so the test proves
        // containment at backend granularity, not merely "other models
        // survive".
        sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             VALUES ($1, 'http://healthy:8000/v1', 'healthy', NULL)",
        )
        .bind(model_id)
        .execute(&pool)
        .await
        .unwrap();

        // An unrelated model, to prove the failure does not take down the
        // rest of the snapshot either.
        let other_model = unique_name("unrelated-model");
        sqlx::query("INSERT INTO models (name) VALUES ($1)")
            .bind(&other_model)
            .execute(&pool)
            .await
            .unwrap();

        let snapshot = build_snapshot(&pool, &key)
            .await
            .expect("one undecryptable backend must not fail the whole snapshot");

        let model = snapshot
            .models
            .iter()
            .find(|m| m.name == broken_model)
            .expect("the model itself must still be in the snapshot");
        assert_eq!(
            model.backends.len(),
            1,
            "the undecryptable backend must be dropped, the healthy one kept"
        );
        assert_eq!(model.backends[0].api_base, "http://healthy:8000/v1");

        assert!(
            snapshot.models.iter().any(|m| m.name == other_model),
            "an unrelated model must be unaffected"
        );
    }
}
