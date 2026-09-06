//! `GET /admin/timeseries` against a real database.
//!
//! # Why this file exists
//!
//! The handler builds one large SQL statement with several CTEs, and
//! `sqlx::query_as` does not check it at compile time. A missing comma
//! between two CTEs compiles, passes every unit test, and fails only when
//! Postgres parses it — which is exactly what happened: the endpoint shipped
//! returning 500 with `syntax error at or near "rej"`, and the only symptom
//! in the UI was a chart quietly showing its "control plane too old"
//! fallback.
//!
//! Nothing here asserts numbers. The property under test is narrow and the
//! one that was missing: **the statement parses and the rows decode into the
//! shape the handler declares.** Every filter combination is exercised
//! because they take different branches through the query.

mod support;

/// Every shape of the query, against whatever data the dev database holds.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_timeseries_query_parses_and_decodes_in_every_filter_combination() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();

    // A model and principal that exist, so the filtered forms take the
    // branch where the predicate is non-NULL rather than short-circuiting.
    let model: Option<String> =
        sqlx::query_scalar("SELECT name FROM provider_models ORDER BY id LIMIT 1")
            .fetch_optional(&pool)
            .await
            .unwrap();
    let principal: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM principals ORDER BY id LIMIT 1")
            .fetch_optional(&pool)
            .await
            .unwrap();

    for (label, model, principal) in [
        ("unfiltered", None, None),
        ("by model", model.clone(), None),
        ("by principal", None, principal),
        ("by both", model, principal),
    ] {
        let rows = fastllm_proxy::control::api::timeseries_for_test(
            &pool,
            3600,
            model.as_deref(),
            principal,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));

        // 24 hours of hourly buckets, inclusive of both ends.
        assert!(
            rows.len() >= 24,
            "{label}: every bucket in the range must be present, including empty \
             ones — a chart drawn from a series with holes interpolates across them"
        );
        for r in &rows {
            assert!(
                r.requests >= 0 && r.upstream_errors >= 0 && r.refused_unattributed >= 0,
                "{label}: counts must never decode negative"
            );
        }
    }
}
