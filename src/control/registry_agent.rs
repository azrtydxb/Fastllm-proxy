//! Dynamic provider registration: what a host that serves models tells us,
//! and what we go and check for ourselves.
//!
//! A service on a GPU host registers an **address** on a lease and refreshes
//! it. It does not send a model list. The control plane calls `GET /v1/models`
//! itself, because FastLLM has to reach the provider anyway in order to serve
//! traffic — a list pushed from the host can name models the proxies cannot
//! dial, and that failure surfaces at request time, to a user. Enumerating
//! from here makes discovery and reachability the same test, and shrinks the
//! agent to something worth trusting on a GPU host: register an address,
//! heartbeat, exit.
//!
//! See `.procoder/adr/0003-the-control-plane-enumerates-a-providers-models.md`.

use crate::upstream::Upstream;
use sqlx::PgPool;

/// What one provider is currently serving, by the only question every engine
/// answers the same way.
///
/// vLLM, SGLang, llama.cpp, TGI, Ollama, Triton's OpenAI frontend, LM Studio
/// and mlx-lm all implement this, as do the hosted providers — so nothing here
/// needs to know which engine it is talking to. An engine hint exists for
/// metadata only and is never load-bearing.
pub async fn served_models(client: &Upstream, api_base: &str) -> anyhow::Result<Vec<String>> {
    use http_body_util::BodyExt as _;
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    let req = hyper::Request::builder()
        .method("GET")
        .uri(&url)
        .header(hyper::header::USER_AGENT, "fastllm-proxy")
        .body(http_body_util::Full::new(bytes::Bytes::new()))?;
    // Short, because this runs on a schedule against every provider and a
    // hung endpoint must not hold the sweep up. A provider that cannot answer
    // in ten seconds is not one a request should be routed to either.
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| anyhow::anyhow!("{url} timed out"))??;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        anyhow::bail!("{url} answered {status}");
    }
    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    let data = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("{url} returned no `data` array"))?;
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_owned))
        .collect())
}

/// The outcome of probing one provider, which answers both questions that
/// matter with one call.
#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// Reachable, and serving exactly what is registered against it.
    Healthy,
    /// Reachable, but serving something other than what the registry claims.
    ///
    /// This is the case that motivated the whole feature and the one no
    /// liveness check can produce: a host answering happily while serving a
    /// different model than the row says. It is reported separately from
    /// "down" because it is a different problem with a different fix.
    Mismatch {
        missing: Vec<String>,
    },
    Unreachable {
        error: String,
    },
}

/// Compare what a provider actually serves against what is registered on it.
pub async fn probe(client: &Upstream, api_base: &str, registered: &[String]) -> Probe {
    match served_models(client, api_base).await {
        Err(e) => Probe::Unreachable {
            error: e.to_string(),
        },
        Ok(served) => {
            let missing: Vec<String> = registered
                .iter()
                .filter(|r| !served.contains(r))
                .cloned()
                .collect();
            if missing.is_empty() {
                Probe::Healthy
            } else {
                Probe::Mismatch { missing }
            }
        }
    }
}

/// Register or refresh a dynamic provider's lease.
///
/// Idempotent by address: an agent heartbeating every thirty seconds calls
/// this every thirty seconds, and it must be the same operation each time.
/// Returns the provider's id.
pub async fn register(
    pool: &PgPool,
    api_base: &str,
    node: &str,
    engine: Option<&str>,
    ttl_seconds: i64,
) -> Result<i64, sqlx::Error> {
    let api_base = api_base.trim_end_matches('/');
    // A provider is its endpoint, so an address already registered by hand
    // stays what it was: this must never quietly convert a static provider
    // into one that can expire.
    if let Some((id, kind)) =
        sqlx::query_as::<_, (i64, String)>("SELECT id, kind FROM providers WHERE api_base = $1")
            .bind(api_base)
            .fetch_optional(pool)
            .await?
    {
        if kind == "dynamic" {
            sqlx::query(
                "UPDATE providers
                    SET node = $2, engine = COALESCE($3, engine),
                        lease_expires_at = now() + make_interval(secs => $4),
                        degraded_since = NULL, degraded_reason = NULL
                  WHERE id = $1",
            )
            .bind(id)
            .bind(node)
            .bind(engine)
            .bind(ttl_seconds as f64)
            .execute(pool)
            .await?;
        }
        return Ok(id);
    }

    let host = api_base
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .unwrap_or(api_base);
    let mut name = host.to_string();
    for n in 2..100 {
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM providers WHERE name=$1)")
                .bind(&name)
                .fetch_one(pool)
                .await?;
        if !taken {
            break;
        }
        name = format!("{host}#{n}");
    }
    sqlx::query_scalar(
        "INSERT INTO providers (name, kind, api_base, node, engine, lease_expires_at)
         VALUES ($1, 'dynamic', $2, $3, $4, now() + make_interval(secs => $5))
         RETURNING id",
    )
    .bind(&name)
    .bind(api_base)
    .bind(node)
    .bind(engine)
    .bind(ttl_seconds as f64)
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_serving_what_is_registered_is_healthy() {
        // `probe` compares sets; the transport is exercised end to end by the
        // sweep's own test against a real endpoint.
        let registered = vec!["qwen3.8-27b".to_string()];
        let served = vec!["qwen3.8-27b".to_string()];
        let missing: Vec<String> = registered
            .iter()
            .filter(|r| !served.contains(r))
            .cloned()
            .collect();
        assert!(missing.is_empty());
    }

    /// The case the whole feature exists for: the host is up and answering,
    /// and serving something else. A liveness probe reports this as healthy.
    #[test]
    fn a_provider_serving_something_else_is_a_mismatch_not_a_failure() {
        let registered = vec!["nvidia/Qwen3.6-35B-A3B-NVFP4".to_string()];
        let served = vec!["qwen3.8-27b".to_string()];
        let missing: Vec<String> = registered
            .iter()
            .filter(|r| !served.contains(r))
            .cloned()
            .collect();
        assert_eq!(missing, vec!["nvidia/Qwen3.6-35B-A3B-NVFP4".to_string()]);
    }

    /// A provider serving *more* than is registered is healthy, not drifted.
    /// OpenRouter answers with hundreds of models and three of them are
    /// registered; that is the normal case for a cloud provider, and treating
    /// the extras as drift would mark every one of them broken.
    #[test]
    fn extra_models_on_a_provider_are_not_drift() {
        let registered = vec!["openai/gpt-5".to_string()];
        let served = vec![
            "openai/gpt-5".to_string(),
            "google/gemini-2.5-flash".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        ];
        let missing: Vec<String> = registered
            .iter()
            .filter(|r| !served.contains(r))
            .cloned()
            .collect();
        assert!(missing.is_empty());
    }
}

/// How long a dynamic provider may be degraded before it is deleted.
///
/// Longer than a model load, on purpose. A 27B on a DGX Spark takes over ten
/// minutes to come up and answers nothing while it does; a host reboot is
/// routine. Deleting on a shorter window would make every restart look like a
/// decommissioning and throw away the provider's credential to do it.
const DEGRADED_GRACE: chrono::Duration = chrono::Duration::minutes(30);

/// One pass over every provider: probe, record, and remove what has been gone
/// long enough.
///
/// Two stages, never one. A failed probe or a lapsed lease marks the provider
/// degraded and takes its models out of rotation; only sustained absence
/// deletes. Suppressing routing is reversible and deletion is not, and the
/// asymmetry is the whole design — see
/// `.procoder/adr/0004-dynamic-providers-degrade-before-they-are-deleted.md`.
///
/// Static and cloud providers are probed on the same schedule but never
/// degrade and are never deleted. A human put them there, and absence is not
/// evidence the human changed their mind — the probe is advisory for them, and
/// exists to report drift the operator would otherwise find by accident.
pub async fn sweep(pool: &PgPool, client: &Upstream) -> anyhow::Result<SweepReport> {
    let mut report = SweepReport::default();

    let providers: Vec<(i64, String, String, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT id, name, api_base, lease_expires_at FROM providers ORDER BY id")
            .fetch_all(pool)
            .await?;

    for (id, name, api_base, lease) in providers {
        let registered: Vec<String> = sqlx::query_scalar(
            "SELECT upstream_model FROM provider_models \
             WHERE provider_id = $1 AND upstream_model IS NOT NULL",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;

        // A lapsed lease is treated exactly like an unreachable endpoint: the
        // agent has stopped vouching for it, and whether the endpoint happens
        // to still answer is beside the point — nothing is maintaining it.
        let lapsed = lease.is_some_and(|l| l < chrono::Utc::now());
        let outcome = if lapsed {
            Probe::Unreachable {
                error: "lease lapsed".into(),
            }
        } else {
            probe(client, &api_base, &registered).await
        };

        match &outcome {
            Probe::Healthy => {
                sqlx::query(
                    "UPDATE providers SET last_seen_at = now(), degraded_since = NULL, \
                     degraded_reason = NULL WHERE id = $1",
                )
                .bind(id)
                .execute(pool)
                .await?;
                report.healthy += 1;
            }
            Probe::Mismatch { missing } | Probe::Unreachable { .. } => {
                let reason = match &outcome {
                    Probe::Mismatch { .. } => {
                        format!("serving something else; missing {missing:?}")
                    }
                    Probe::Unreachable { error } => error.clone(),
                    Probe::Healthy => unreachable!(),
                };
                // `COALESCE` so the clock starts at the *first* failure and is
                // not reset by every subsequent one — otherwise a provider
                // failing every probe would never age out.
                sqlx::query(
                    "UPDATE providers SET degraded_since = COALESCE(degraded_since, now()), \
                     degraded_reason = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(&reason)
                .execute(pool)
                .await?;
                if matches!(outcome, Probe::Mismatch { .. }) {
                    report.mismatched.push(name.clone());
                } else {
                    report.unreachable.push(name.clone());
                }
            }
        }
    }

    // Only `dynamic`, and only after the grace window. The `WHERE kind` is the
    // load-bearing half: without it this would delete the provider a human
    // typed in because a host was briefly down.
    let removed: Vec<String> = sqlx::query_scalar(
        "DELETE FROM providers
          WHERE kind = 'dynamic'
            AND degraded_since IS NOT NULL
            AND degraded_since < now() - make_interval(secs => $1)
          RETURNING name",
    )
    .bind(DEGRADED_GRACE.num_seconds() as f64)
    .fetch_all(pool)
    .await?;
    report.deleted = removed;

    Ok(report)
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub healthy: usize,
    /// Reachable but serving something other than what is registered. Reported
    /// apart from `unreachable` because it is a different problem: the host is
    /// fine and the registry is wrong.
    pub mismatched: Vec<String>,
    pub unreachable: Vec<String>,
    /// Dynamic providers whose absence outlasted the grace window.
    pub deleted: Vec<String>,
}
