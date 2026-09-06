//! The roll-up that keeps `usage_events` from growing without bound.
//!
//! Two properties matter, and both are ways this could quietly destroy data
//! rather than fail loudly:
//!
//! **Lossless.** Every count that goes into a bucket must come back out of
//! it. The roll-up deletes the rows it summarises, so a summary that drops a
//! column, mis-filters a refusal, or forgets the served/failed split
//! destroys the only copy. Nothing later would notice — the chart would just
//! show less traffic in the past than actually happened, which reads as a
//! quiet period rather than as a bug.
//!
//! **Idempotent.** It is not assumed to run exactly once per hour. A control
//! plane down for a day rolls several windows on its next tick, and a retry
//! after a partial failure re-runs a window that partly landed. Running it
//! twice must not double the totals.

mod support;

use support::TestCleanup;

/// Same shape as the other suites': this runs against the shared kw dev
/// database, so every name a test creates has to be its own.
fn unique_name(tag: &str) -> String {
    format!(
        "{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Insert one usage row `days` in the past, with an explicit outcome.
///
/// Nine parameters, which clippy dislikes and which is right here: every one
/// is a column the roll-up must carry through, and collapsing them into a
/// struct would hide the very fields this test exists to vary.
#[allow(clippy::too_many_arguments)]
async fn insert_event(
    pool: &sqlx::PgPool,
    principal_id: uuid::Uuid,
    provider_model_id: uuid::Uuid,
    days_ago: i64,
    status: i16,
    refusal: Option<&str>,
    prompt: i64,
    completion: i64,
    cost: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO usage_events
             (principal_id, provider_model_id, prompt_tokens, completion_tokens, at,
              duration_ms, status, refusal, usage_reported, cost_micros)
         VALUES ($1, $2, $3, $4, now() - make_interval(days => $5), 100, $6, $7, $8, $9)",
    )
    .bind(principal_id)
    .bind(provider_model_id)
    .bind(prompt)
    .bind(completion)
    .bind(days_ago as i32)
    .bind(status)
    .bind(refusal)
    .bind(refusal.is_none())
    .bind(cost)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn rolling_up_preserves_every_count_and_can_run_twice() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();

    let model = unique_name("retention-model");
    let principal = unique_name("retention-principal");
    let _cleanup = TestCleanup::new()
        .track_prefix("provider_models", "name", "retention-model")
        .track_prefix("principals", "name", "retention-principal");

    let provider_model_id: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO provider_models (name) VALUES ($1) RETURNING id")
            .bind(&model)
            .fetch_one(&pool)
            .await
            .unwrap();
    let principal_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO principals (name, kind) VALUES ($1, 'service_account') RETURNING id",
    )
    .bind(&principal)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Well outside the 90-day window, so the roll-up must take all of them:
    // four served, one upstream error, one of each refusal kind.
    for _ in 0..4 {
        insert_event(
            &pool,
            principal_id,
            provider_model_id,
            120,
            200,
            None,
            10,
            20,
            Some(50),
        )
        .await;
    }
    insert_event(
        &pool,
        principal_id,
        provider_model_id,
        120,
        500,
        None,
        0,
        0,
        None,
    )
    .await;
    for (status, kind) in [
        (403i16, "authorisation"),
        (429, "rate_limit"),
        (402, "budget"),
        (502, "no_backend"),
    ] {
        insert_event(
            &pool,
            principal_id,
            provider_model_id,
            120,
            status,
            Some(kind),
            0,
            0,
            None,
        )
        .await;
    }

    // And one inside the window, which must survive untouched — the roll-up
    // must not reach forward into rows the raw table is still meant to hold.
    insert_event(
        &pool,
        principal_id,
        provider_model_id,
        1,
        200,
        None,
        7,
        9,
        Some(11),
    )
    .await;

    let cutoff_sql = "at < now() - make_interval(days => 90)";
    let before: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM usage_events WHERE principal_id = $1 AND {cutoff_sql}"
    ))
    .bind(principal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, 9, "fixture should have 9 rows past the cutoff");

    fastllm_proxy::control::api::roll_up_and_prune_usage_for_test(&pool)
        .await
        .expect("roll-up");

    let raw_left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_events WHERE principal_id = $1 AND at < now() - make_interval(days => 90)",
    )
    .bind(principal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(raw_left, 0, "old raw rows must be gone once summarised");

    let recent_left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_events WHERE principal_id = $1 AND at >= now() - make_interval(days => 90)",
    )
    .bind(principal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recent_left, 1, "rows inside the window must be untouched");

    let row: (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        // `sum()` over a bigint yields numeric in Postgres, not bigint --
        // the same cast the usage handler needs, and the same runtime decode
        // failure without it.
        "SELECT sum(requests)::bigint, sum(upstream_errors)::bigint,
                sum(refused_authorisation)::bigint, sum(refused_rate_limit)::bigint,
                sum(refused_budget)::bigint, sum(refused_no_backend)::bigint,
                sum(prompt_tokens)::bigint, sum(completion_tokens)::bigint,
                sum(cost_micros)::bigint
         FROM usage_rollup_hourly WHERE principal_id = $1",
    )
    .bind(principal_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.0, 9, "every request must survive the fold");
    assert_eq!(row.1, 1, "the upstream error must survive");
    assert_eq!(
        (row.2, row.3, row.4, row.5),
        (1, 1, 1, 1),
        "each refusal kind"
    );
    assert_eq!(row.6, 40, "prompt tokens summed across the four served");
    assert_eq!(row.7, 80, "completion tokens summed");
    assert_eq!(row.8, 200, "cost summed");

    // Running again must be a no-op, not a doubling. Nothing is left older
    // than the cutoff, so a second pass has nothing to add — the property
    // being pinned is that `ON CONFLICT DO UPDATE` adding to the existing
    // bucket cannot re-add rows that are already gone.
    fastllm_proxy::control::api::roll_up_and_prune_usage_for_test(&pool)
        .await
        .expect("second roll-up");

    let after: i64 = sqlx::query_scalar(
        "SELECT sum(requests)::bigint FROM usage_rollup_hourly WHERE principal_id = $1",
    )
    .bind(principal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, 9, "a second run must not double the totals");

    sqlx::query("DELETE FROM usage_rollup_hourly WHERE principal_id = $1")
        .bind(principal_id)
        .execute(&pool)
        .await
        .unwrap();
}

/// Deleting a model must not delete what it was billed for.
///
/// `usage_events.provider_model_id` was `ON DELETE CASCADE`, which was a reasonable
/// reading while models were deleted rarely and by hand — migration 0005 argued
/// exactly that. It stops being reasonable the moment anything deletes them on
/// a schedule: swap a model off a host and that week's inference disappears
/// from usage and spend, with no error, because the cascade is doing what it
/// was told.
///
/// This pins the row surviving *and* still naming what served it, because a
/// row that survives with nothing but a NULL id is not usable evidence of
/// anything.
#[tokio::test]
#[ignore = "requires postgres"]
async fn deleting_a_model_keeps_the_usage_it_was_billed_for() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set; skipping");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();

    let model = unique_name("deleted-model");
    let principal = unique_name("deleted-model-principal");
    // The model is deleted by the test itself; only the principal needs
    // cleaning up, and the usage rows go with it.
    let _cleanup = TestCleanup::new()
        .track_prefix("provider_models", "name", "deleted-model")
        .track_prefix("principals", "name", "deleted-model-principal");

    let provider: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO providers (name, api_base) VALUES ($1, 'http://gone:8000/v1') RETURNING id",
    )
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    let provider_model_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO provider_models (name, provider_id, upstream_model) \
         VALUES ($1, $2, 'gone') RETURNING id",
    )
    .bind(&model)
    .bind(provider)
    .fetch_one(&pool)
    .await
    .unwrap();
    let principal_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO principals (name, kind) VALUES ($1, 'service_account') RETURNING id",
    )
    .bind(&principal)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO usage_events \
             (principal_id, provider_model_id, model_name, provider_name, prompt_tokens, \
              completion_tokens, at, usage_reported) \
         VALUES ($1, $2, $3, $3, 100, 200, now(), true)",
    )
    .bind(principal_id)
    .bind(provider_model_id)
    .bind(&model)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("DELETE FROM provider_models WHERE id = $1")
        .bind(provider_model_id)
        .execute(&pool)
        .await
        .unwrap();

    let (rows, named, id_cleared): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), \
                count(*) FILTER (WHERE model_name = $2), \
                count(*) FILTER (WHERE provider_model_id IS NULL) \
           FROM usage_events WHERE principal_id = $1",
    )
    .bind(principal_id)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(rows, 1, "the usage row must outlive the model");
    assert_eq!(named, 1, "and must still say what it was billed under");
    assert_eq!(id_cleared, 1, "the id is cleared, not the row");

    // The provider is a separate lifetime: deleting a model must not take it,
    // because other models may still be on it.
    let provider_left: i64 = sqlx::query_scalar("SELECT count(*) FROM providers WHERE id = $1")
        .bind(provider)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(provider_left, 1);

    sqlx::query("DELETE FROM providers WHERE id = $1")
        .bind(provider)
        .execute(&pool)
        .await
        .unwrap();
}
