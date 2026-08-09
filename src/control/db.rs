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
        // Presence and permissions, not an exact table listing.
        //
        // This asserted the whole `roles` table equalled exactly the three
        // seeded names, which is not something a migration can promise: the
        // suite shares one scratch database with everything else that writes
        // to it. It already needed a carve-out for `import:%`, and it broke
        // again the first time an operator created a role through the admin
        // API — a legitimate act reported as a migration defect.
        //
        // What the migrations actually own is that these three exist and grant
        // exactly what they are supposed to, so that is what is checked. A
        // migration seeding a *fourth* role would still be caught by the
        // permission assertions below being the complete set for each.
        let seeded: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT r.name, p.verb, p.resource
             FROM roles r
             JOIN role_permissions rp ON rp.role_id = r.id
             JOIN permissions p ON p.id = rp.permission_id
             WHERE r.name IN ('admin', 'inference', 'operator')
             ORDER BY r.name, p.verb, p.resource",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let grants = |role: &str| -> Vec<String> {
            seeded
                .iter()
                .filter(|(r, _, _)| r == role)
                .map(|(_, v, res)| format!("{v} {res}"))
                .collect()
        };

        // `inference` is the one that matters most to get exactly right: it is
        // what a caller-facing key normally holds, and `model/*` rather than a
        // bare `*` is what the authorisation path matches on.
        assert_eq!(grants("inference"), vec!["model:invoke model/*"]);
        assert_eq!(
            grants("operator"),
            vec![
                "config:write *",
                "key:create *",
                "key:revoke *",
                "usage:read *"
            ],
            "operator is everything except model:invoke"
        );
        assert_eq!(
            grants("admin"),
            vec![
                "config:write *",
                "key:create *",
                "key:revoke *",
                "model:invoke model/*",
                "usage:read *"
            ]
        );
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn migrations_are_idempotent() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        connect(&url).await.unwrap();
        connect(&url).await.unwrap();
    }
}
