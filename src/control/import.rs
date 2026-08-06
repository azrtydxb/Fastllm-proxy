//! Seeds the database from a LiteLLM-format config.
//!
//! The migration path off a file-driven deployment, and the reason the
//! LiteLLM-compatibility promise survives the move to a control plane.

use crate::config::FileConfig;
use crate::control::secrets::{self, EncryptionKey};
use sqlx::PgPool;

/// What an import run actually did — every field is a per-run delta, the
/// same way `backends` always was. A prior revision reported `models` as the
/// *total* row count in the table, which read as "N models were just
/// created" on a re-import against a database that already had rows; that
/// was wrong and has been fixed. Re-running the same or an edited file over
/// an already-seeded database should now print zeroes for anything that was
/// already there.
///
/// No `keys` field: a LiteLLM-format config has no key material to import —
/// `import` seeds `models`/`model_backends` only, never `api_keys` — so a
/// field that could only ever read zero would just be a second thing to keep
/// truthful for no information gained. An earlier revision carried one
/// anyway and printed it as "0 new key(s)" on every run, which read as a
/// claim that key import was attempted and always finds nothing, rather
/// than what was actually true: it was never attempted at all.
pub struct ImportSummary {
    pub models: usize,
    pub backends: usize,
}

/// Seed `models`/`model_backends` from a LiteLLM-format config.
///
/// Idempotent on `(model_id, api_base, upstream_model)`: re-running this over
/// the same file, or an edited one, adds rows without duplicating existing
/// backends. That is what makes it safe to run against a live deployment's
/// ConfigMap repeatedly while migrating it onto the control plane, rather
/// than requiring one careful one-shot cutover.
pub async fn import(
    pool: &PgPool,
    cfg: &FileConfig,
    key: &EncryptionKey,
) -> anyhow::Result<ImportSummary> {
    let mut tx = pool.begin().await?;
    let mut models = 0usize;
    let mut backends = 0usize;

    for entry in &cfg.model_list {
        let name = &entry.model_name;
        // Look up before inserting (rather than INSERT ... ON CONFLICT and
        // checking `xmax`) so a genuine per-run count of *new* models falls
        // out directly, the same way `backends` is counted below — no
        // separate "count what changed" query needed.
        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM models WHERE name = $1")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
        let model_id = match existing {
            Some(id) => id,
            None => {
                let id: i64 =
                    sqlx::query_scalar("INSERT INTO models (name) VALUES ($1) RETURNING id")
                        .bind(name)
                        .fetch_one(&mut *tx)
                        .await?;
                models += 1;
                id
            }
        };

        let api_base = entry.litellm_params.api_base.trim_end_matches('/');
        let upstream = entry.litellm_params.upstream_model(name);
        // Idempotent on (model, api_base, upstream_model) so re-running import
        // over an edited file adds without duplicating.
        //
        // upstream_api_key is encrypted at rest with AES-256-GCM
        // (`control::secrets`) before it ever reaches Postgres — see that
        // module's doc comment for exactly what this does and does not
        // protect. `effective_api_key` already strips LiteLLM's
        // `not-needed`/`sk-1234`-style placeholders, so `None` here means
        // "this backend genuinely has no credential", not "encryption was
        // skipped".
        let encrypted_key = entry
            .litellm_params
            .effective_api_key()
            .map(|k| secrets::encrypt(key, &k))
            .transpose()?;
        let inserted = sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             SELECT $1, $2, $3, $4
             WHERE NOT EXISTS (
               SELECT 1 FROM model_backends
               WHERE model_id = $1 AND api_base = $2 AND upstream_model = $3)",
        )
        .bind(model_id)
        .bind(api_base)
        .bind(&upstream)
        .bind(encrypted_key)
        .execute(&mut *tx)
        .await?;
        backends += inserted.rows_affected() as usize;
    }

    tx.commit().await?;
    Ok(ImportSummary { models, backends })
}

/// One-shot migration for rows written before encryption-at-rest existed
/// (`migrations/0004_encrypted_upstream_api_key.sql`).
///
/// Chosen over a format that silently distinguishes plaintext from
/// ciphertext on every read: `build_snapshot` and `import` both need
/// `upstream_api_key` to mean one thing, "an `encrypt`-produced blob", on
/// every code path, permanently — teaching the hot read path to also
/// tolerate raw plaintext bytes forever, for the sake of rows that exist on
/// at most a handful of developer scratch databases (the live cluster has
/// no control-plane database yet at all), is a worse trade than a command
/// an operator runs once after upgrading.
///
/// Detection still uses the self-describing format, just here instead of on
/// every read: a row is left alone if it already decrypts under `key` (see
/// `secrets::is_encrypted` for why that is a safe classifier), and
/// re-encrypted in place otherwise, on the assumption that anything which
/// doesn't decrypt is the raw plaintext bytes `import` used to write
/// directly. Safe to run more than once — an already-migrated database is a
/// no-op — and safe to run against a database with no plaintext rows at all.
///
/// Generic over `sqlx::Acquire` (both `&PgPool` and `&mut Transaction` work)
/// rather than pinned to `&PgPool`, purely so a test can drive it inside one
/// transaction and prove the migration is atomic — `build_snapshot` decrypts
/// every non-null `upstream_api_key` row it sees, so any window in which a
/// pre-migration plaintext row is visible on the shared connection pool but
/// not yet migrated is a window in which an unrelated `build_snapshot` call
/// against the same database would fail. Every production call site still
/// passes `&PgPool` exactly as before.
pub async fn reencrypt_plaintext_backends<'a, A>(
    conn: A,
    key: &EncryptionKey,
) -> anyhow::Result<usize>
where
    A: sqlx::Acquire<'a, Database = sqlx::Postgres>,
{
    let mut conn = conn.acquire().await?;
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT id, upstream_api_key FROM model_backends WHERE upstream_api_key IS NOT NULL",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut migrated = 0usize;
    for (id, blob) in rows {
        if secrets::is_encrypted(key, &blob) {
            continue;
        }
        let plaintext = String::from_utf8(blob).map_err(|_| {
            anyhow::anyhow!(
                "model_backends.id={id}: upstream_api_key is neither valid ciphertext under the \
                 given key nor valid UTF-8 plaintext; refusing to guess"
            )
        })?;
        let reencrypted = secrets::encrypt(key, &plaintext)?;
        sqlx::query("UPDATE model_backends SET upstream_api_key = $1 WHERE id = $2")
            .bind(reencrypted)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        migrated += 1;
    }
    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch database used for these tests is shared across the whole
    /// `cargo test` invocation (and, in practice, with whatever else happens
    /// to be pointed at it). The brief's original tests started with
    /// `TRUNCATE models, model_backends CASCADE`, which is fine in isolation
    /// but races with any other test truncating the same tables concurrently
    /// — including each other, once `models` stopped being a total count and
    /// started being a per-run delta (a truncate from the sibling test
    /// landing between this test's two `import` calls made the second call
    /// see an empty table and report a spurious "new" model). Uniquely-named
    /// models sidestep the whole class of race instead of trying to order
    /// two `#[tokio::test]`s that `cargo test` is free to run in parallel.
    fn unique_name(tag: &str) -> String {
        format!(
            "{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Shared with `control::api::tests` — see `secrets::test_key` for why
    /// every DB-backed test in `control::*` must use the same key rather
    /// than each picking its own.
    use secrets::test_key;

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_the_deployed_config_creates_models_and_backends() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();

        let name = unique_name("qwen3");
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n\
             \x20 - model_name: {name}\n\
             \x20   litellm_params: {{ model: openai/{name}, api_base: http://a:8000/v1 }}\n\
             \x20 - model_name: {name}\n\
             \x20   litellm_params: {{ model: openai/{name}, api_base: http://b:8000/v1 }}\n"
        ))
        .unwrap();

        let summary = import(&pool, &cfg, &test_key()).await.unwrap();
        // Two entries sharing a model_name are one pool with two backends.
        assert_eq!(summary.models, 1);
        assert_eq!(summary.backends, 2);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_twice_does_not_duplicate() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();

        let name = unique_name("m");
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n  - model_name: {name}\n    litellm_params: {{ api_base: http://a:8000/v1 }}\n"
        ))
        .unwrap();
        let first = import(&pool, &cfg, &test_key()).await.unwrap();
        assert_eq!(first.models, 1, "first run creates the one new model");
        let second = import(&pool, &cfg, &test_key()).await.unwrap();
        // `models` is a per-run delta, same as `backends`: nothing new was
        // created on the second run, so it reports zero, not the table total.
        assert_eq!(second.models, 0, "re-import must not report a new model");

        let model_id: i64 = sqlx::query_scalar("SELECT id FROM models WHERE name = $1")
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM model_backends WHERE model_id = $1")
                .bind(model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "re-import must be idempotent");
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn the_stored_column_is_not_the_plaintext_credential_and_build_snapshot_recovers_it() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = test_key();

        let name = unique_name("secret-backend");
        let plaintext_credential = "sk-do-not-leak-this-upstream-token";
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n  - model_name: {name}\n    litellm_params: {{ api_base: http://a:8000/v1, api_key: {plaintext_credential} }}\n"
        ))
        .unwrap();

        import(&pool, &cfg, &key).await.unwrap();

        let stored: Vec<u8> = sqlx::query_scalar(
            "SELECT upstream_api_key FROM model_backends b
             JOIN models m ON m.id = b.model_id
             WHERE m.name = $1",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();

        // What's in the column must not be the plaintext credential...
        assert_ne!(stored, plaintext_credential.as_bytes());
        assert!(String::from_utf8_lossy(&stored)
            .find(plaintext_credential)
            .is_none());
        // ...and must not be readable without the key.
        let wrong_key = secrets::EncryptionKey::from_hex(&"cd".repeat(secrets::KEY_LEN)).unwrap();
        assert!(secrets::decrypt(&wrong_key, &stored).is_err());

        // The right key recovers it, both directly...
        assert_eq!(
            secrets::decrypt(&key, &stored).unwrap(),
            plaintext_credential
        );
        // ...and through the path the proxy actually uses.
        let snapshot = crate::control::build::build_snapshot(&pool, &key)
            .await
            .unwrap();
        let model = snapshot.models.iter().find(|m| m.name == name).unwrap();
        assert_eq!(
            model.backends[0].api_key.as_deref(),
            Some(plaintext_credential)
        );
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn reencrypt_migrates_a_plaintext_row_and_is_idempotent() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = test_key();

        let name = unique_name("legacy-plaintext");

        // Inserting the plaintext row and migrating it happen inside one
        // transaction, committed only once both are done. `build_snapshot`
        // (used by other tests sharing this same scratch database, and by
        // the periodic rebuild in `control::api`) decrypts every non-null
        // `upstream_api_key` row it sees table-wide; committing the raw
        // plaintext row on its own, even briefly, would be a real window in
        // which a concurrent `build_snapshot` call legitimately fails on
        // this test's fixture data. Read-committed isolation means no other
        // connection ever sees a row that was inserted and migrated inside
        // the same uncommitted transaction.
        let mut tx = pool.begin().await.unwrap();
        let model_id: i64 =
            sqlx::query_scalar("INSERT INTO models (name) VALUES ($1) RETURNING id")
                .bind(&name)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        // Simulate a row written by the pre-encryption code path: raw
        // plaintext bytes, exactly as the old `import` stored them.
        sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             VALUES ($1, 'http://legacy:8000/v1', 'legacy-model', $2)",
        )
        .bind(model_id)
        .bind(b"sk-legacy-plaintext-token".to_vec())
        .execute(&mut *tx)
        .await
        .unwrap();

        let migrated = reencrypt_plaintext_backends(&mut tx, &key).await.unwrap();
        assert!(migrated >= 1);
        tx.commit().await.unwrap();

        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT upstream_api_key FROM model_backends WHERE model_id = $1")
                .bind(model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(stored, b"sk-legacy-plaintext-token".to_vec());
        assert_eq!(
            secrets::decrypt(&key, &stored).unwrap(),
            "sk-legacy-plaintext-token"
        );

        // Running it again must be a no-op: the row is already ciphertext.
        let migrated_again = reencrypt_plaintext_backends(&pool, &key).await.unwrap();
        let stored_again: Vec<u8> =
            sqlx::query_scalar("SELECT upstream_api_key FROM model_backends WHERE model_id = $1")
                .bind(model_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored, stored_again,
            "already-migrated row must be left alone"
        );
        let _ = migrated_again;
    }
}
