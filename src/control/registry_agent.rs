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

pub use crate::engine_metrics::{engine_load, EngineLoad};
use crate::upstream::Upstream;
use sqlx::PgPool;
use uuid::Uuid;

/// What one provider is currently serving, by the only question every engine
/// answers the same way.
///
/// vLLM, SGLang, llama.cpp, TGI, Ollama, Triton's OpenAI frontend, LM Studio
/// and mlx-lm all implement this, as do the hosted providers — so nothing here
/// needs to know which engine it is talking to. An engine hint exists for
/// metadata only and is never load-bearing.
pub async fn served_models(client: &Upstream, api_base: &str) -> anyhow::Result<Vec<String>> {
    served_models_as(client, api_base, None).await
}

/// The credential a provider presents upstream, in the shape the header wants.
///
/// The same three fields `registry::Backend` composes for the request path, so
/// a probe authenticates exactly as a real request to that provider would —
/// which is the only way a probe can answer "does this key work".
pub struct Credential<'a> {
    pub header: &'a str,
    pub scheme: Option<&'a str>,
    pub key: &'a str,
}

/// `GET /v1/models`, presenting a credential when one is given.
///
/// Unauthenticated was the only mode until this existed, and it made the sweep
/// wrong for any provider whose model list needs a key: the probe got a 401,
/// the provider was marked degraded, and nothing about it was actually wrong.
/// It also made validating a credential impossible, since the call never
/// carried one.
pub async fn served_models_as(
    client: &Upstream,
    api_base: &str,
    credential: Option<Credential<'_>>,
) -> anyhow::Result<Vec<String>> {
    use http_body_util::BodyExt as _;
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    let mut builder = hyper::Request::builder()
        .method("GET")
        .uri(&url)
        .header(hyper::header::USER_AGENT, "fastllm-proxy");
    if let Some(c) = credential {
        let value = match c.scheme {
            Some(s) if !s.is_empty() => format!("{s} {}", c.key),
            // Raw key, no prefix: what `x-api-key`/`x-goog-api-key` want.
            _ => c.key.to_string(),
        };
        builder = builder.header(c.header.to_ascii_lowercase(), value);
    }
    let req = builder.body(http_body_util::Full::new(bytes::Bytes::new()))?;
    // Short, because this runs on a schedule against every provider and a
    // hung endpoint must not hold the sweep up. A provider that cannot answer
    // in ten seconds is not one a request should be routed to either.
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), client.request(req))
        .await
        .map_err(|_| anyhow::anyhow!("{url} timed out"))??;
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("reading {url}: {e}"))?
        .to_bytes();
    if !status.is_success() {
        anyhow::bail!("{url} answered {status}");
    }
    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    // `data` is OpenAI's shape and `models` is Gemini's. Accepting both is
    // what lets one probe answer for every protocol here, rather than the
    // native ones being unverifiable.
    let data = parsed
        .get("data")
        .or_else(|| parsed.get("models"))
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("{url} returned no model list"))?;
    Ok(data
        .iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(|i| i.as_str())
                .map(str::to_owned)
        })
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

/// Registered models this provider is not currently serving.
///
/// One direction only, deliberately. A provider serving *more* than is
/// registered is healthy: OpenRouter answers with hundreds of models and three
/// of them are registered, so treating the extras as drift would mark every
/// cloud provider broken.
fn missing_from(registered: &[String], served: &[String]) -> Vec<String> {
    registered
        .iter()
        .filter(|r| !served.contains(r))
        .cloned()
        .collect()
}

/// Compare what a provider actually serves against what is registered on it.
pub async fn probe(client: &Upstream, api_base: &str, registered: &[String]) -> Probe {
    match served_models(client, api_base).await {
        Err(e) => Probe::Unreachable {
            error: e.to_string(),
        },
        Ok(served) => {
            let missing = missing_from(registered, &served);
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
/// Re-attach targets whose model has come back.
///
/// `provider_model_id` is `ON DELETE SET NULL`, so a model the registrar
/// removed leaves its targets naming what they want with no id. Routing still
/// finds them by that name — that is migration 0036's whole point — but a
/// target held together by a string is one a later rename would break. Putting
/// the id back closes that window, so "renames do not break links" holds for a
/// model that has been away and come back, not only for one that never left.
///
/// Matched on the model name alone, which is exact: `provider_models.name` is
/// unique, and since migration 0045 one model served by two hosts is one row
/// with two attachments rather than two rows a name could not tell apart. The
/// provider name this used to also match on existed only for that ambiguity
/// and no longer exists.
pub async fn relink_targets(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut relinked = 0;
    for table in ["frontend_model_defaults", "rule_targets"] {
        relinked += sqlx::query(&format!(
            "UPDATE {table} t SET provider_model_id = pm.id
               FROM provider_models pm
              WHERE t.provider_model_id IS NULL
                AND t.target_model_name = pm.name"
        ))
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(relinked)
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

pub async fn register(
    pool: &PgPool,
    api_base: &str,
    node: &str,
    name: Option<&str>,
    engine: Option<&str>,
    ttl_seconds: i64,
) -> Result<Uuid, sqlx::Error> {
    let api_base = api_base.trim_end_matches('/');
    // A provider is its endpoint, so an address already registered by hand
    // stays what it was: this must never quietly convert a static provider
    // into one that can expire.
    if let Some((id, kind)) =
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, kind FROM providers WHERE api_base = $1")
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
            // The agent owns a dynamic provider's name, so a changed one
            // renames it. Best-effort on purpose: a name another provider
            // already holds must not fail the heartbeat, because the lease is
            // what keeps the endpoint routable and a naming collision is not a
            // reason to let it lapse. The provider keeps the name it has and
            // the operator sees the old one, which is visible and recoverable
            // — unlike a host that quietly stopped renewing.
            if let Some(name) = name {
                let renamed = sqlx::query(
                    "UPDATE providers SET name = $2 WHERE id = $1 AND name IS DISTINCT FROM $2",
                )
                .bind(id)
                .bind(name)
                .execute(pool)
                .await;
                match renamed {
                    // Nothing to carry onto the targets: a target names a
                    // model, and which providers serve that model is the
                    // model's business (migration 0045).
                    Ok(_) => {}
                    Err(e) if is_unique_violation(&e) => {
                        tracing::warn!(
                            provider = %id,
                            requested = name,
                            "agent asked for a provider name another provider already holds; \
                             keeping the current one"
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        return Ok(id);
    }

    let host = api_base
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .unwrap_or(api_base);
    // The agent's name if it gave one, the address if it did not.
    let mut name = name.unwrap_or(host).to_string();
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
    use super::missing_from;

    fn owned(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_provider_serving_what_is_registered_is_healthy() {
        assert!(missing_from(&owned(&["qwen3.8-27b"]), &owned(&["qwen3.8-27b"])).is_empty());
    }

    /// The case the whole feature exists for: the host is up and answering,
    /// and serving something else. A liveness probe calls this healthy.
    #[test]
    fn a_provider_serving_something_else_is_a_mismatch_not_a_failure() {
        assert_eq!(
            missing_from(
                &owned(&["nvidia/Qwen3.6-35B-A3B-NVFP4"]),
                &owned(&["qwen3.8-27b"])
            ),
            owned(&["nvidia/Qwen3.6-35B-A3B-NVFP4"])
        );
    }

    /// A provider serving *more* than is registered is healthy, not drifted.
    /// OpenRouter answers with hundreds and three are registered; treating the
    /// extras as drift would mark every cloud provider broken.
    #[test]
    fn extra_models_on_a_provider_are_not_drift() {
        assert!(missing_from(
            &owned(&["openai/gpt-5"]),
            &owned(&[
                "openai/gpt-5",
                "google/gemini-2.5-flash",
                "anthropic/claude-sonnet-4-5"
            ])
        )
        .is_empty());
    }

    /// A provider with nothing registered on it yet is healthy, not drifted —
    /// the state every dynamic provider is in for its first sweep.
    #[test]
    fn a_provider_with_nothing_registered_is_healthy() {
        assert!(missing_from(&[], &owned(&["qwen3.8-27b"])).is_empty());
    }
}

/// Bring a dynamic provider's models in line with what it actually serves.
///
/// Adds what is newly served and removes what is not, for `dynamic` providers
/// only — a static or cloud provider's models are a human's list, and a model
/// missing from one `GET /v1/models` is not permission to delete it.
///
/// **A learned model is inventory, not an exposure.** Nothing here creates a
/// frontend model for it, so a model that appears on a registered host reaches
/// nobody until an operator points a frontend model at it. That is the
/// closed-by-default half of moving authorisation to frontend models: a host
/// starting an unrelated model must not hand existing principals access to it
/// (`.procoder/adr/0002-authorisation-moves-to-the-frontend-model.md`).
pub async fn reconcile_models(
    pool: &PgPool,
    provider_id: Uuid,
    provider_name: &str,
    served: &[String],
) -> Result<(usize, usize), sqlx::Error> {
    let known: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT mb.provider_model_id, COALESCE(mb.upstream_model, m.name) \
         FROM model_backends mb \
         JOIN provider_models m ON m.id = mb.provider_model_id \
         WHERE mb.provider_id = $1",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await?;

    let mut added = 0usize;
    for upstream in served {
        if known.iter().any(|(_, u)| u == upstream) {
            continue;
        }
        // A second host serving a model this deployment already knows is the
        // normal case, and since migration 0045 the right answer is another
        // attachment on that model rather than a second model named
        // `model@host`. Two Sparks serving one model then land in one pool,
        // which is the whole point -- `router.rs` can send a conversation back
        // to the box that already has its prefix cached.
        //
        // Only where every existing attachment is on a *dynamic* provider,
        // though. An agent is a thing that registered itself; letting one
        // graft a host onto a model an operator configured by hand would hand
        // every principal already granted that model a route to a machine
        // nobody vetted, which is exactly what ADR 0002 closes. Against an
        // operator-configured model the old qualified name is still used, so
        // the host shows up as its own model for a human to look at.
        let existing: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT m.id, NOT EXISTS ( \
                 SELECT 1 FROM model_backends mb \
                 JOIN providers p ON p.id = mb.provider_id \
                 WHERE mb.provider_model_id = m.id AND p.kind <> 'dynamic') \
             FROM provider_models m WHERE m.name = $1",
        )
        .bind(upstream)
        .fetch_optional(pool)
        .await?;

        let model_id = match existing {
            Some((id, true)) => id,
            // Taken by a model an operator configured, or not taken at all.
            other => {
                let name = if other.is_some() {
                    format!("{upstream}@{provider_name}")
                } else {
                    upstream.clone()
                };
                let created: Option<Uuid> = sqlx::query_scalar(
                    "INSERT INTO provider_models (name) VALUES ($1) \
                     ON CONFLICT (name) DO NOTHING RETURNING id",
                )
                .bind(&name)
                .fetch_optional(pool)
                .await?;
                match created {
                    Some(id) => id,
                    // Lost a race with another agent; it made the row.
                    None => continue,
                }
            }
        };

        sqlx::query(
            "INSERT INTO model_backends (provider_model_id, provider_id, upstream_model) \
             VALUES ($1, $2, $3) ON CONFLICT (provider_model_id, provider_id) DO NOTHING",
        )
        .bind(model_id)
        .bind(provider_id)
        .bind(upstream)
        .execute(pool)
        .await?;
        added += 1;
    }

    // Gone from the provider means gone from the registry -- but only its own
    // models, and only for a provider whose whole point is that it is
    // maintained automatically. Usage rows survive this (migration 0031) and a
    // frontend model pointing at it keeps its target by name (0036), so the
    // delete is recoverable in every way that matters.
    //
    // The *attachment* goes, not the model. Since migration 0045 a model can be
    // served by several providers, so one host dropping it says nothing about
    // the others -- deleting the model here would take it away from every
    // provider still serving it. A model left with no attachments is simply not
    // routable, which is a state the request path and the UI already show.
    let removed = sqlx::query(
        "DELETE FROM model_backends mb \
          USING provider_models m \
          WHERE mb.provider_model_id = m.id \
            AND mb.provider_id = $1 \
            AND NOT (COALESCE(mb.upstream_model, m.name) = ANY($2))",
    )
    .bind(provider_id)
    .bind(served)
    .execute(pool)
    .await?
    .rows_affected() as usize;

    Ok((added, removed))
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

    let providers: Vec<(Uuid, String, String, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT id, name, api_base, lease_expires_at FROM providers ORDER BY id")
            .fetch_all(pool)
            .await?;

    for (id, name, api_base, lease) in providers {
        let kind: String = sqlx::query_scalar("SELECT kind FROM providers WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?;
        let registered: Vec<String> = sqlx::query_scalar(
            "SELECT COALESCE(mb.upstream_model, m.name) FROM model_backends mb \
             JOIN provider_models m ON m.id = mb.provider_model_id \
             WHERE mb.provider_id = $1",
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

        // A degraded provider is described by *why*, because "unreachable" and
        // "serving something else" are different problems with different
        // fixes — only the second means the host is fine and the registry is
        // wrong, which is the case this whole feature exists for.
        let degraded_reason = match outcome {
            Probe::Healthy => None,
            Probe::Mismatch { ref missing } => {
                report.mismatched.push(name.clone());
                Some(format!("serving something else; missing {missing:?}"))
            }
            Probe::Unreachable { ref error } => {
                report.unreachable.push(name.clone());
                Some(error.clone())
            }
        };

        // Load is read whether or not the provider is dynamic, and whether or
        // not it is degraded: an endpoint under so much load that it stopped
        // answering the model list is exactly when its queue depth is worth
        // seeing. A provider with no `/metrics` leaves the columns NULL rather
        // than zero, so "idle" and "does not say" stay distinguishable.
        match engine_load(client, &api_base).await {
            Ok(load) => {
                sqlx::query(
                    "UPDATE providers SET engine_running = $2, engine_waiting = $3, \
                     engine_kv_cache = $4, engine_load_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(load.running as i32)
                .bind(load.waiting as i32)
                .bind(load.kv_cache)
                .execute(pool)
                .await?;
                report.measured += 1;
            }
            Err(_) => {
                sqlx::query(
                    "UPDATE providers SET engine_running = NULL, engine_waiting = NULL, \
                     engine_kv_cache = NULL, engine_load_at = NULL WHERE id = $1",
                )
                .bind(id)
                .execute(pool)
                .await?;
            }
        }

        // Only a dynamic provider learns. A cloud provider answering with four
        // hundred models must not have four hundred rows created for it, and a
        // static provider's list is a human's.
        if kind == "dynamic" && degraded_reason.is_none() {
            if let Ok(served) = served_models(client, &api_base).await {
                let (added, removed) = reconcile_models(pool, id, &name, &served).await?;
                report.models_added += added;
                report.models_removed += removed;
            }
        }

        match degraded_reason {
            None => {
                sqlx::query(
                    "UPDATE providers SET last_seen_at = now(), degraded_since = NULL, \
                     degraded_reason = NULL WHERE id = $1",
                )
                .bind(id)
                .execute(pool)
                .await?;
                report.healthy += 1;
            }
            Some(reason) => {
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
    /// How many providers answered `/metrics`. Counted because the useful
    /// question when a screen shows nothing is "did anyone report", not
    /// "was this one provider quiet".
    pub measured: usize,
    pub healthy: usize,
    /// Reachable but serving something other than what is registered. Reported
    /// apart from `unreachable` because it is a different problem: the host is
    /// fine and the registry is wrong.
    pub mismatched: Vec<String>,
    pub unreachable: Vec<String>,
    /// Dynamic providers whose absence outlasted the grace window.
    pub deleted: Vec<String>,
    pub models_added: usize,
    pub models_removed: usize,
}
