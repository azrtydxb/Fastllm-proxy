//! Seeds the database from a LiteLLM-format config.
//!
//! The migration path off a file-driven deployment, and the reason the
//! LiteLLM-compatibility promise survives the move to a control plane.

use crate::config::FileConfig;
use sqlx::PgPool;

/// What an import run did. `models` is the total number of models now in the
/// table (a re-import over the same file reports the same total, not zero
/// new rows), while `backends` is the number of backend rows *this run*
/// actually inserted — the two counts answer different questions ("what does
/// the database look like now" vs "what did this run do") and are not meant
/// to be comparable to each other.
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
    let mut backends = 0usize;

    for entry in &cfg.model_list {
        let name = &entry.model_name;
        let model_id: i64 = sqlx::query_scalar(
            "INSERT INTO models (name) VALUES ($1)
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
             RETURNING id",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;

        let api_base = entry.litellm_params.api_base.trim_end_matches('/');
        let upstream = entry.litellm_params.upstream_model(name);
        // Idempotent on (model, api_base, upstream_model) so re-running import
        // over an edited file adds without duplicating.
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

    let models = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM models")
        .fetch_one(&mut *tx)
        .await? as usize;

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

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_the_deployed_config_creates_models_and_backends() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        sqlx::query("TRUNCATE models, model_backends CASCADE")
            .execute(&pool)
            .await
            .unwrap();

        let cfg: crate::config::FileConfig = serde_yaml::from_str(
            "model_list:\n\
             \x20 - model_name: qwen3\n\
             \x20   litellm_params: { model: openai/qwen3, api_base: http://a:8000/v1 }\n\
             \x20 - model_name: qwen3\n\
             \x20   litellm_params: { model: openai/qwen3, api_base: http://b:8000/v1 }\n",
        )
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
        sqlx::query("TRUNCATE models, model_backends CASCADE")
            .execute(&pool)
            .await
            .unwrap();
        let cfg: crate::config::FileConfig = serde_yaml::from_str(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://a:8000/v1 }\n",
        )
        .unwrap();
        import(&pool, &cfg).await.unwrap();
        let second = import(&pool, &cfg).await.unwrap();
        assert_eq!(second.models, 1);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_backends")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "re-import must be idempotent");
    }
}
