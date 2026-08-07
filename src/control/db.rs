use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect and bring the schema up to date.
///
/// Migrations run on every start rather than as a separate step: there is one
/// writer, the migrations are small, and a deployment that cannot self-migrate
/// is a deployment that fails at 3am.
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires Postgres. Run with:
    ///   docker compose up -d postgres
    ///   DATABASE_URL=postgres://fastllm:fastllm@localhost:5432/fastllm \
    ///     cargo test --features control -- --ignored
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn migrations_apply_and_seed_the_default_roles() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = connect(&url).await.unwrap();
        // Still an exact whole-table assertion, minus the one namespace that
        // is not the migration's to control: `control::import` creates an
        // `import:<principal>` role per imported `auth.keys` entry (see
        // `import::import_role_name`), and this suite shares one scratch
        // database, so a role left behind by an import test is not evidence
        // that a migration seeded something it shouldn't have. Excluding the
        // prefix keeps this strict about everything the migrations *do* own.
        let roles: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM roles WHERE name NOT LIKE 'import:%' ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(roles, vec!["admin", "inference", "operator"]);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn migrations_are_idempotent() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        connect(&url).await.unwrap();
        connect(&url).await.unwrap();
    }
}
