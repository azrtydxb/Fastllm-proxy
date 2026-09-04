//! Where prices come from, so nobody has to type them.
//!
//! Three sources, and they are not equal:
//!
//! 1. **What the provider says it charged.** Returned in `usage.cost` by
//!    OpenRouter, read on the request path (`tail_buffer`), and authoritative:
//!    it is the amount actually billed, it accounts for cache discounts and for
//!    a routed alias serving a different model per request, and it never goes
//!    stale. Nothing in this module competes with it.
//! 2. **A published catalogue**, for the models a provider prices but does not
//!    report per request. OpenRouter publishes all of its models
//!    unauthenticated; that is what `Source::OpenRouter` reads.
//! 3. **The community dataset**, for everyone else. OpenAI, Anthropic and
//!    Google all expose a `/models` endpoint with no prices in it at all —
//!    their prices exist only on a web page — so the ecosystem relies on
//!    LiteLLM's `model_prices_and_context_window.json`. It is a third party's
//!    file: correct in practice, occasionally stale, and a dependency on
//!    somebody else's maintenance. Worth saying out loud rather than treating
//!    it as an oracle.
//!
//! # The unit
//!
//! Both sources quote **per token**, as decimals. This crate stores
//! **micro-units per million tokens**, so the conversion is `× 10^12` — and it
//! is the one piece of arithmetic here worth testing, because getting it wrong
//! by a factor of a thousand produces numbers that still look plausible.

use std::collections::HashMap;

/// Prices for one model, in micro-units per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub input_per_mtok: i64,
    pub output_per_mtok: i64,
}

/// Where to read prices from.
// `Deserialize` so the same choice can arrive from the CLI or from a request
// body, spelled the same way in both: `--source open-router`, `"open-router"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// `openrouter.ai/api/v1/models` — every model it fronts, unauthenticated.
    OpenRouter,
    /// LiteLLM's community-maintained catalogue, which covers the providers
    /// that publish no machine-readable prices at all.
    Catalogue,
    /// Both, with the catalogue filling what OpenRouter does not list.
    Both,
}

pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
pub const CATALOGUE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Per-token decimal to micro-units per million tokens.
///
/// `0.00001` per token is $10 per million; stored as `10_000_000`.
///
/// Rounded rather than truncated, and saturating rather than wrapping: a
/// catalogue entry with an absurd value should produce a clamped number an
/// operator can see, not a negative one that makes a budget behave backwards.
fn per_token_to_per_mtok(per_token: f64) -> Option<i64> {
    if !per_token.is_finite() || per_token < 0.0 {
        return None;
    }
    let scaled = (per_token * 1e12).round();
    if scaled > i64::MAX as f64 {
        return Some(i64::MAX);
    }
    Some(scaled as i64)
}

/// Prices from OpenRouter's model list.
///
/// Keyed by the id OpenRouter uses (`anthropic/claude-sonnet-4`), which is
/// exactly what a backend's `upstream_model` holds for an OpenRouter backend.
pub fn parse_openrouter(body: &[u8]) -> anyhow::Result<HashMap<String, Price>> {
    #[derive(serde::Deserialize)]
    struct Response {
        data: Vec<Model>,
    }
    #[derive(serde::Deserialize)]
    struct Model {
        id: String,
        pricing: Pricing,
    }
    #[derive(serde::Deserialize)]
    struct Pricing {
        // Strings, not numbers, in OpenRouter's response.
        prompt: Option<String>,
        completion: Option<String>,
    }

    let parsed: Response = serde_json::from_slice(body)?;
    Ok(parsed
        .data
        .into_iter()
        .filter_map(|m| {
            let input = per_token_to_per_mtok(m.pricing.prompt?.parse().ok()?)?;
            let output = per_token_to_per_mtok(m.pricing.completion?.parse().ok()?)?;
            Some((
                m.id,
                Price {
                    input_per_mtok: input,
                    output_per_mtok: output,
                },
            ))
        })
        .collect())
}

/// Prices from the community catalogue.
///
/// Keyed by its own model names, which are usually the provider's own
/// (`gpt-4o`) and sometimes prefixed. Both spellings are kept so a lookup can
/// try the backend's `upstream_model` as-is.
pub fn parse_catalogue(body: &[u8]) -> anyhow::Result<HashMap<String, Price>> {
    let parsed: HashMap<String, serde_json::Value> = serde_json::from_slice(body)?;
    Ok(parsed
        .into_iter()
        .filter_map(|(name, entry)| {
            // The file carries a `sample_spec` documentation entry alongside
            // the real ones.
            if name == "sample_spec" {
                return None;
            }
            let input = per_token_to_per_mtok(entry.get("input_cost_per_token")?.as_f64()?)?;
            let output = per_token_to_per_mtok(entry.get("output_cost_per_token")?.as_f64()?)?;
            Some((
                name,
                Price {
                    input_per_mtok: input,
                    output_per_mtok: output,
                },
            ))
        })
        .collect())
}

/// Find a price for a backend's upstream model name.
///
/// Tries the name as written first, then without a `vendor/` prefix — the
/// catalogue lists `gpt-4o` where an OpenRouter backend calls it
/// `openai/gpt-4o`, and an operator should not have to know which.
pub fn lookup<'a>(prices: &'a HashMap<String, Price>, upstream_model: &str) -> Option<&'a Price> {
    prices.get(upstream_model).or_else(|| {
        upstream_model
            .split_once('/')
            .and_then(|(_, bare)| prices.get(bare))
    })
}

/// What a sync did.
pub struct SyncReport {
    pub updated: usize,
    pub skipped: usize,
    pub unmatched: usize,
    /// Model name and the price chosen, for a caller that wants to show it.
    pub changes: Vec<(String, Price)>,
}

/// Fetch the catalogues and fill in prices.
///
/// Shared by `fastllm-proxy sync-prices` and `POST /admin/prices/sync` rather
/// than implemented twice — two copies of "which price wins" would drift, and
/// the answer is the interesting part.
pub async fn sync(
    pool: &sqlx::PgPool,
    client: &crate::upstream::Upstream,
    source: Source,
    overwrite: bool,
    dry_run: bool,
) -> anyhow::Result<SyncReport> {
    let prices = fetch(client, source).await?;

    // One row per model: its price comes from whatever it actually calls
    // upstream. Since migration 0029 a provider model has one provider and one
    // `upstream_model`, so this no longer fans out — a model that used to
    // appear once per backend now appears once.
    let rows: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT m.id, m.name, m.upstream_model, m.input_price_per_mtok
         FROM models m ORDER BY m.name",
    )
    .fetch_all(pool)
    .await?;

    let mut report = SyncReport {
        updated: 0,
        skipped: 0,
        unmatched: 0,
        changes: Vec::new(),
    };
    let mut seen = std::collections::HashSet::new();
    for (id, name, upstream_model, existing) in rows {
        if !seen.insert(id) {
            continue;
        }
        if existing.is_some() && !overwrite {
            report.skipped += 1;
            continue;
        }
        let Some(price) = upstream_model
            .as_deref()
            .and_then(|u| lookup(&prices, u).copied())
        else {
            report.unmatched += 1;
            continue;
        };
        if !dry_run {
            sqlx::query(
                "UPDATE models SET input_price_per_mtok = $2, output_price_per_mtok = $3 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(price.input_per_mtok)
            .bind(price.output_per_mtok)
            .execute(pool)
            .await?;
        }
        report.updated += 1;
        report.changes.push((name, price));
    }
    Ok(report)
}

/// Read the configured sources, tolerating one being unreachable.
///
/// Filling in half the prices beats filling in none because GitHub was briefly
/// unavailable; only every source failing is an error.
async fn fetch(
    client: &crate::upstream::Upstream,
    source: Source,
) -> anyhow::Result<HashMap<String, Price>> {
    let mut prices = HashMap::new();
    let mut errors = Vec::new();

    if matches!(source, Source::OpenRouter | Source::Both) {
        match get(client, OPENROUTER_MODELS_URL)
            .await
            .and_then(|b| parse_openrouter(&b))
        {
            Ok(p) => prices.extend(p),
            Err(e) => errors.push(format!("openrouter: {e}")),
        }
    }
    if matches!(source, Source::Catalogue | Source::Both) {
        match get(client, CATALOGUE_URL)
            .await
            .and_then(|b| parse_catalogue(&b))
        {
            // Inserted without replacing: OpenRouter's own published price
            // beats a third party's copy of it.
            Ok(p) => {
                for (k, v) in p {
                    prices.entry(k).or_insert(v);
                }
            }
            Err(e) => errors.push(format!("catalogue: {e}")),
        }
    }
    if prices.is_empty() {
        anyhow::bail!("no prices could be fetched ({})", errors.join("; "));
    }
    Ok(prices)
}

async fn get(client: &crate::upstream::Upstream, url: &str) -> anyhow::Result<bytes::Bytes> {
    use http_body_util::BodyExt as _;
    let req = hyper::Request::builder()
        .method("GET")
        .uri(url)
        .header(hyper::header::USER_AGENT, "fastllm-proxy")
        .body(http_body_util::Full::new(bytes::Bytes::new()))?;
    let resp = tokio::time::timeout(std::time::Duration::from_secs(30), client.request(req))
        .await
        .map_err(|_| anyhow::anyhow!("fetching {url} timed out"))??;
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
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion worth testing: wrong by a factor of a thousand still
    /// looks like a plausible price.
    #[test]
    fn per_token_decimals_become_micro_units_per_million_tokens() {
        // $10 per million tokens.
        assert_eq!(per_token_to_per_mtok(0.00001), Some(10_000_000));
        // $2.50 per million, the shape the catalogue uses.
        assert_eq!(per_token_to_per_mtok(2.5e-06), Some(2_500_000));
        // $0.15 per million, a cheap model.
        assert_eq!(per_token_to_per_mtok(1.5e-07), Some(150_000));
        // Free is a price, and must not become "unpriced".
        assert_eq!(per_token_to_per_mtok(0.0), Some(0));
    }

    #[test]
    fn a_nonsense_price_is_refused_rather_than_stored() {
        assert_eq!(per_token_to_per_mtok(-1.0), None);
        assert_eq!(per_token_to_per_mtok(f64::NAN), None);
        assert_eq!(per_token_to_per_mtok(f64::INFINITY), None);
        // Absurd but finite: clamped to something visible, never wrapped into
        // a negative that would make a budget behave backwards.
        assert_eq!(per_token_to_per_mtok(1e30), Some(i64::MAX));
    }

    #[test]
    fn openrouter_prices_are_read_from_its_model_list() {
        let body = br#"{"data":[
            {"id":"anthropic/claude-opus","pricing":{"prompt":"0.00001","completion":"0.00005"}},
            {"id":"free/model","pricing":{"prompt":"0","completion":"0"}},
            {"id":"broken/model","pricing":{"completion":"0.1"}}
        ]}"#;
        let prices = parse_openrouter(body).unwrap();
        assert_eq!(
            prices["anthropic/claude-opus"],
            Price {
                input_per_mtok: 10_000_000,
                output_per_mtok: 50_000_000
            }
        );
        assert_eq!(prices["free/model"].input_per_mtok, 0);
        assert!(
            !prices.contains_key("broken/model"),
            "half a price is not a price"
        );
    }

    #[test]
    fn the_catalogue_is_read_and_its_documentation_entry_skipped() {
        let body = br#"{
            "sample_spec": {"input_cost_per_token": 0.0, "output_cost_per_token": 0.0},
            "gpt-4o": {"input_cost_per_token": 2.5e-06, "output_cost_per_token": 1e-05},
            "no-price": {"max_tokens": 100}
        }"#;
        let prices = parse_catalogue(body).unwrap();
        assert_eq!(
            prices["gpt-4o"],
            Price {
                input_per_mtok: 2_500_000,
                output_per_mtok: 10_000_000
            }
        );
        assert!(
            !prices.contains_key("sample_spec"),
            "documentation, not data"
        );
        assert!(!prices.contains_key("no-price"));
    }

    /// An operator should not have to know that the catalogue calls it
    /// `gpt-4o` where their OpenRouter backend calls it `openai/gpt-4o`.
    #[test]
    fn a_vendor_prefix_is_tried_both_ways() {
        let mut prices = HashMap::new();
        prices.insert(
            "gpt-4o".to_string(),
            Price {
                input_per_mtok: 1,
                output_per_mtok: 2,
            },
        );
        assert!(lookup(&prices, "openai/gpt-4o").is_some());
        assert!(lookup(&prices, "gpt-4o").is_some());
        assert!(lookup(&prices, "openai/gpt-5").is_none());
    }
}
