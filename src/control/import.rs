//! Seeds the database from a LiteLLM-format config.
//!
//! The migration path off a file-driven deployment, and the reason the
//! LiteLLM-compatibility promise survives the move to a control plane.

use crate::config::FileConfig;
use sqlx::PgPool;

/// What an import run actually did — every field is a per-run delta, the
/// same way `backends` always was. A prior revision reported `models` as the
/// *total* row count in the table, which read as "N models were just
/// created" on a re-import against a database that already had rows; that
/// was wrong and has been fixed. Re-running the same or an edited file over
/// an already-seeded database should now print zeroes for anything that was
/// already there.
pub struct ImportSummary {
    pub models: usize,
    pub backends: usize,
    pub keys: usize,
}

/// Seed `models`/`model_backends` from a LiteLLM-format config.
///
/// Idempotent on `(model_id, api_base, upstream_model)`: re-running this over
/// the same file, or an edited one, adds rows without duplicating existing
/// backends. That is what makes it safe to run against a live deployment's
/// ConfigMap repeatedly while migrating it onto the control plane, rather
/// than requiring one careful one-shot cutover.
pub async fn import(pool: &PgPool, cfg: &FileConfig) -> anyhow::Result<ImportSummary> {
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
        // upstream_api_key is stored as-is, NOT encrypted at rest (the schema
        // comment used to claim otherwise; see migrations/0002). Anyone who
        // can read this table can read upstream credentials in plaintext.
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
        .bind(
            entry
                .litellm_params
                .effective_api_key()
                .map(|k| k.into_bytes()),
        )
        .execute(&mut *tx)
        .await?;
        backends += inserted.rows_affected() as usize;
    }

    tx.commit().await?;
    Ok(ImportSummary {
        models,
        backends,
        keys: 0,
    })
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

        let summary = import(&pool, &cfg).await.unwrap();
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
        let first = import(&pool, &cfg).await.unwrap();
        assert_eq!(first.models, 1, "first run creates the one new model");
        let second = import(&pool, &cfg).await.unwrap();
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
}
