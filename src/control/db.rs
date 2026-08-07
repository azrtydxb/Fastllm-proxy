use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect and bring the schema up to date.
///
/// Migrations run on every start rather than as a separate step: there is one
/// writer, the migrations are small, and a deployment that cannot self-migrate
/// is a deployment that fails at 3am.
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    connect_with(url, default_max_connections()).await
}

/// Pool size, from `FASTLLM_DATABASE_MAX_CONNECTIONS`, default 8.
///
/// Configurable because the ceiling is shared and easy to hit without noticing:
/// Postgres allows 100 connections by default, and every process that talks to
/// it — each control plane, each proxy replica, each test that spawns one —
/// takes a whole pool, not a connection. Fourteen concurrent end-to-end tests
/// at 8 apiece is 112, and the symptom is every one of them failing at once
/// with `PoolTimedOut`, which reads as a code failure rather than a capacity
/// one. It cost two debugging sessions before this flag existed.
pub fn default_max_connections() -> u32 {
    std::env::var("FASTLLM_DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(8)
}

pub async fn connect_with(url: &str, max_connections: u32) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        // Hand connections back when the work stops. Without this a process
        // that goes quiet — or one killed mid-test — keeps its whole pool
        // parked until the server times the sockets out, which is how 86 idle
        // connections accumulated against a limit of 100.
        .min_connections(0)
        .idle_timeout(std::time::Duration::from_secs(30))
        // A hard ceiling on how long any one connection lives, so a pool
        // cannot slowly fill with sockets a restarted Postgres no longer
        // knows about.
        .max_lifetime(std::time::Duration::from_secs(1800))
        .connect(url)
        .await?;
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
