//! Turns database rows into the snapshot the data plane consumes.
//!
//! All the expensive work happens here, once per change, so that the request
//! path is a set lookup: roles are resolved to permissions, permissions to
//! model names, and wildcards expanded against the known model list.

use crate::control::secrets::{self, EncryptionKey};
use crate::protocol::Protocol;
use crate::routing::{MatchConditionJson, RoutingRule, VirtualModelDef, WeightedTarget};
use crate::snapshot::PromptClassDef;
use crate::snapshot::{BackendDef, Budget, KeyEntry, ModelDef, Principal, Snapshot};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

/// Resolve `model:invoke` permissions into a concrete set of model names.
///
/// `model/*` short-circuits to `allow_all` rather than materialising every
/// name, so a grant stays correct when a model is added later. Any other verb
/// is an administrative permission and has no place in the request path.
pub fn flatten_grants(
    perms: &[(String, String)],
    all_models: &[String],
) -> (HashSet<String>, bool) {
    let mut set = HashSet::new();
    for (verb, resource) in perms {
        if verb != "model:invoke" {
            continue;
        }
        let Some(pattern) = resource.strip_prefix("model/") else {
            continue;
        };
        if pattern == "*" {
            return (HashSet::new(), true);
        }
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                set.extend(all_models.iter().filter(|m| m.starts_with(prefix)).cloned())
            }
            None => {
                if all_models.iter().any(|m| m == pattern) {
                    set.insert(pattern.to_string());
                }
            }
        }
    }
    (set, false)
}

/// Build a full snapshot from the current database state.
///
/// This runs one permissions query per principal rather than a single join
/// across principals/roles/permissions. That is deliberate, not an
/// oversight: this runs once per change (never on the request path) against
/// tens of principals, so the N+1 shape costs nothing that matters and stays
/// far easier to read than the joined equivalent.
///
/// `key` decrypts `model_backends.upstream_api_key` (see `control::secrets`
/// for the format and exactly what encryption at rest here does and does
/// not protect). The snapshot this returns still carries the credential in
/// usable plaintext form — the proxy has to present it to the backend — so
/// this decrypts the database's copy, it does not add protection to the
/// snapshot itself. `/snapshot` must be TLS wherever a backend has a real
/// credential, same as before this module existed.
pub async fn build_snapshot(pool: &PgPool, key: &EncryptionKey) -> anyhow::Result<Snapshot> {
    build_snapshot_with(pool, key, embedder()).await
}

/// The same build, with an embedder for prompt-class centroids.
///
/// `embed` is passed in rather than constructed here so that this module does
/// not depend on the `classifier` feature: a control plane built without it
/// still builds snapshots, and simply publishes classes with empty centroids —
/// which the request path then ignores, rather than matching them at some
/// arbitrary distance.
pub async fn build_snapshot_with(
    pool: &PgPool,
    key: &EncryptionKey,
    embed: Option<&dyn PromptClassEmbedder>,
) -> anyhow::Result<Snapshot> {
    let model_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM models ORDER BY name")
            .fetch_all(pool)
            .await?;
    let all_names: Vec<String> = model_rows.iter().map(|(_, n)| n.clone()).collect();

    type BackendRow = (
        i64,
        String,
        String,
        Option<Vec<u8>>,
        String,
        String,
        Option<String>,
        Option<i32>,
    );
    let backend_rows: Vec<BackendRow> = sqlx::query_as(
        "SELECT model_id, api_base, upstream_model, upstream_api_key, protocol, auth_header, \
         auth_scheme, default_max_tokens FROM model_backends",
    )
    .fetch_all(pool)
    .await?;

    let mut models = Vec::new();
    for (id, name) in &model_rows {
        let mut backends = Vec::new();
        for (_, base, upstream, encrypted_key, protocol, auth_header, auth_scheme, max_tokens) in
            backend_rows.iter().filter(|(mid, ..)| mid == id)
        {
            // A decrypt failure here is contained to this one backend, not
            // propagated as a `build_snapshot` error. It used to be: one row
            // that failed to decrypt (a partially completed key rotation, a
            // row `reencrypt_plaintext_backends` never reached, anything not
            // matching `key`) failed the *entire* snapshot — a hard exit on
            // the startup path (`main.rs`'s `EncryptionKey::from_env`
            // sibling calls all propagate `?`), crash-looping the whole
            // control plane and taking every model offline over one bad row.
            // Dropping just the affected backend and logging it loudly is
            // fail-closed on the credential that actually cannot be used,
            // without failing closed on every model that has nothing to do
            // with it. If this was a model's only backend, that model ends
            // up with none, which is exactly the "no healthy backend" case
            // the proxy already has to handle for other reasons (every
            // backend down, none configured).
            let api_key = match encrypted_key.as_ref() {
                None => None,
                Some(blob) => match secrets::decrypt(key, blob) {
                    Ok(plaintext) => Some(plaintext),
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            model = %name,
                            api_base = %base,
                            "dropping backend: upstream_api_key failed to decrypt; excluded from \
                             this snapshot rather than failing the whole rebuild. Re-encrypt it \
                             with the current FASTLLM_ENCRYPTION_KEY (see \
                             `reencrypt-backends`/`is_encrypted`) or delete and recreate it \
                             through the admin API."
                        );
                        continue;
                    }
                },
            };
            // The column is CHECK-constrained to the names `Protocol::parse`
            // knows, so this only fails if the two drift apart — in which case
            // dropping the backend here surfaces it in the control plane's log
            // rather than in a proxy's, where the cause would be much further
            // from the change that caused it.
            let Some(protocol) = Protocol::parse(protocol) else {
                tracing::error!(
                    protocol = %protocol,
                    model = %name,
                    api_base = %base,
                    "dropping backend: protocol is not one this build implements"
                );
                continue;
            };
            backends.push(BackendDef {
                api_base: base.trim_end_matches('/').to_string(),
                upstream_model: upstream.clone(),
                api_key,
                protocol,
                auth_header: auth_header.clone(),
                auth_scheme: auth_scheme.clone(),
                default_max_tokens: max_tokens.map(|n| n as u32),
            });
        }
        models.push(ModelDef {
            name: name.clone(),
            backends,
        });
    }

    let principal_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM principals WHERE NOT disabled")
            .fetch_all(pool)
            .await?;

    // One query for every configured limit, not one per principal: this
    // table has at most one row per principal (see migrations/0009) and the
    // whole point of pre-resolving into the snapshot is to spend queries
    // here, once per rebuild, rather than on the request path.
    let limit_rows: Vec<(i64, Option<i32>, Option<i32>)> =
        sqlx::query_as("SELECT principal_id, requests_per_min, tokens_per_min FROM limits")
            .fetch_all(pool)
            .await?;
    let limits_by_principal: HashMap<i64, crate::limiter::Limits> = limit_rows
        .into_iter()
        .map(|(pid, requests, tokens)| {
            (
                pid,
                crate::limiter::Limits {
                    // Negative would only reach here via hand-written SQL —
                    // the admin route and the migration's own CHECK
                    // constraint both refuse it — but clamping to `None`
                    // (unlimited) rather than panicking on the cast is the
                    // same "contain the failure to the one bad row" pattern
                    // this module already applies to an undecryptable
                    // backend and an unparseable routing rule below.
                    requests_per_min: requests.filter(|v| *v > 0).map(|v| v as u32),
                    tokens_per_min: tokens.filter(|v| *v > 0).map(|v| v as u32),
                },
            )
        })
        .collect();

    let budgets_by_principal = roll_over_and_load_budgets(pool).await?;

    let mut principals = HashMap::new();
    for (id, name) in principal_rows {
        // One query per principal: see the module-level rationale above.
        let perms: Vec<(String, String)> = sqlx::query_as(
            "SELECT p.verb, p.resource FROM permissions p
             JOIN role_permissions rp ON rp.permission_id = p.id
             JOIN principal_roles pr  ON pr.role_id = rp.role_id
             WHERE pr.principal_id = $1",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;
        // Deliberately *not* widened to include virtual model names — see
        // the authorisation decision in `proxy::resolve_target_model`'s doc
        // comment: a virtual model routes access, it does not grant it, so
        // `allowed_models` only ever needs to answer "may this principal
        // invoke this *concrete* model", the same question it has always
        // answered.
        let (allowed_models, allow_all) = flatten_grants(&perms, &all_names);

        // The raw role names, separate from `allowed_models`: a routing
        // rule's caller condition (`crate::routing::CallerMatch`) matches on
        // "does this principal hold role X", which flattening would throw
        // away.
        let role_names: Vec<String> = sqlx::query_scalar(
            "SELECT r.name FROM principal_roles pr JOIN roles r ON r.id = pr.role_id
             WHERE pr.principal_id = $1",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;

        principals.insert(
            id as u64,
            Principal {
                id: id as u64,
                name,
                allowed_models,
                allow_all,
                roles: role_names.into_iter().collect(),
                limits: limits_by_principal.get(&id).copied(),
                budget: budgets_by_principal.get(&id).copied(),
            },
        );
    }

    let virtual_models = build_virtual_models(pool).await?;

    type KeyRow = (Vec<u8>, i64, Option<chrono::DateTime<chrono::Utc>>, bool);
    let key_rows: Vec<KeyRow> =
        sqlx::query_as("SELECT hash, principal_id, expires_at, disabled FROM api_keys")
            .fetch_all(pool)
            .await?;

    let mut keys = HashMap::new();
    for (hash, principal, expires, disabled) in key_rows {
        let Ok(hash): Result<[u8; 32], _> = hash.try_into() else {
            continue;
        };
        keys.insert(
            hash,
            KeyEntry {
                principal: principal as u64,
                expires_at: expires.map(|d| d.into()),
                disabled,
            },
        );
    }

    // The version is a clock the control plane owns; the proxy only compares it.
    let version: i64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT")
        .fetch_one(pool)
        .await?;

    let prompt_classes = build_prompt_classes(pool, embed).await?;
    let fallback_model: Option<String> =
        sqlx::query_scalar("SELECT name FROM models WHERE is_fallback LIMIT 1")
            .fetch_optional(pool)
            .await?;

    Ok(Snapshot {
        version: version as u64,
        keys,
        principals,
        models,
        virtual_models,
        prompt_classes,
        fallback_model,
        open: false,
    })
}

/// Turns a class's example prompts into one normalised centroid.
///
/// A trait rather than a concrete type so `build` stays free of the
/// `classifier` feature; `main.rs` supplies the implementation when the build
/// has one.
/// Process-wide embedder, registered once at startup.
///
/// A global rather than a parameter threaded through every `build_snapshot`
/// call site: the model is genuinely process-level state — one set of weights,
/// loaded once, shared by the startup build and every admin-triggered rebuild —
/// and threading it through would put a `classifier`-feature type in the
/// signature of a function that must compile without that feature.
static EMBEDDER: std::sync::OnceLock<Box<dyn PromptClassEmbedder>> = std::sync::OnceLock::new();

/// Register the embedder. Later calls are ignored; there is one model.
pub fn set_prompt_class_embedder(embedder: Box<dyn PromptClassEmbedder>) {
    let _ = EMBEDDER.set(embedder);
}

fn embedder() -> Option<&'static dyn PromptClassEmbedder> {
    EMBEDDER.get().map(|b| b.as_ref())
}

pub trait PromptClassEmbedder: Send + Sync {
    /// Embed each prompt for the fast tier. Per prompt rather than pre-averaged
    /// so the admin API can hold one example out and score it against the rest
    /// — the difference between reporting that a class exists and reporting
    /// whether it works.
    fn fast(&self, prompts: &[String]) -> Option<Vec<Vec<f32>>>;
    /// The same for the refined tier. `None` when this build or this deployment
    /// has no transformer, which is the common case.
    fn refined(&self, prompts: &[String]) -> Option<Vec<Vec<f32>>>;
}

/// The registered embedder, for callers outside snapshot building.
pub fn prompt_class_embedder() -> Option<&'static dyn PromptClassEmbedder> {
    embedder()
}

/// Default margin when a class does not set one.
///
/// 0.10 is where `potion-code-16M` classified 88% of real traffic at 99.9%
/// accuracy on the coding-versus-everything split (see docs/classifier.md). It
/// is a starting point an operator should tune per class against their own
/// examples, not a universal constant — which is exactly why the column is
/// nullable rather than defaulted in the schema.
const DEFAULT_MIN_MARGIN: f32 = 0.10;

async fn build_prompt_classes(
    pool: &PgPool,
    embed: Option<&dyn PromptClassEmbedder>,
) -> anyhow::Result<Vec<PromptClassDef>> {
    let rows: Vec<(i64, String, String, Option<f32>)> =
        sqlx::query_as("SELECT id, name, tier, min_margin FROM prompt_classes ORDER BY name")
            .fetch_all(pool)
            .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let examples: Vec<(i64, String)> =
        sqlx::query_as("SELECT class_id, prompt FROM prompt_class_examples ORDER BY id")
            .fetch_all(pool)
            .await?;
    let refines: Vec<(i64, String)> =
        sqlx::query_as("SELECT class_id, refines FROM prompt_class_refines")
            .fetch_all(pool)
            .await?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, name, tier, min_margin) in rows {
        let prompts: Vec<String> = examples
            .iter()
            .filter(|(cid, _)| *cid == id)
            .map(|(_, p)| p.clone())
            .collect();

        // A class with no examples has nothing to be the mean of. Published
        // with an empty centroid so the admin API can still show it — the
        // request path drops it — rather than silently vanishing from a list
        // the operator is looking at.
        let centroid = if prompts.is_empty() {
            tracing::warn!(class = %name, "prompt class has no example prompts; it cannot match anything");
            Vec::new()
        } else {
            let each = match embed {
                Some(e) if tier == "refined" => e.refined(&prompts),
                Some(e) => e.fast(&prompts),
                None => None,
            };
            each.and_then(|v| crate::vector::centroid(&v))
                .unwrap_or_default()
        };
        if centroid.is_empty() && !prompts.is_empty() {
            tracing::warn!(
                class = %name, tier = %tier,
                "prompt class could not be embedded; excluded from routing. Is the classifier \
                 feature built and --classifier-model set?"
            );
        }

        out.push(PromptClassDef {
            name: name.clone(),
            tier,
            centroid,
            min_margin: min_margin.unwrap_or(DEFAULT_MIN_MARGIN),
            refines: refines
                .iter()
                .filter(|(cid, _)| *cid == id)
                .map(|(_, r)| r.clone())
                .collect(),
        });
    }
    Ok(out)
}

/// P3's three fixed window lengths. Deliberately *not* calendar arithmetic —
/// `'monthly'` is a rolling 30-day period, not "resets on the 1st" — because
/// getting true calendar-month rollover right (28/29/30/31-day months,
/// timezone-of-record for "the 1st") is real complexity this design does not
/// need to take on to satisfy "a monthly budget resets". An operator who
/// needs billing-calendar precision is better served by reading
/// `usage_events` directly than by this coarse a mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetWindow {
    Daily,
    Weekly,
    Monthly,
}

impl BudgetWindow {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }

    fn duration(self) -> chrono::Duration {
        match self {
            Self::Daily => chrono::Duration::days(1),
            Self::Weekly => chrono::Duration::days(7),
            Self::Monthly => chrono::Duration::days(30),
        }
    }
}

/// Pure rollover decision, factored out of the database call so it can be
/// tested without Postgres: has this budget's window elapsed as of `now`,
/// and if so, what does the reset row look like?
///
/// Only ever advances *one* window forward, not however many were missed
/// while nobody looked — a budget dormant for three missed months resets to
/// `window_start + 30d` once, not to "now" and not to three resets in a row.
/// The next `build_snapshot` (run on the same `snapshot_rebuild_interval` as
/// everything else) catches it up further if it is still elapsed, which for
/// any realistic rebuild interval is instantaneous from an operator's point
/// of view — the alternative, looping here to catch up in one call, buys
/// nothing but a more complicated function for a case that is not
/// meaningfully slower without it.
fn rolled_over(
    window_start: chrono::DateTime<chrono::Utc>,
    window: BudgetWindow,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let elapsed_at = window_start + window.duration();
    if now >= elapsed_at {
        Some(elapsed_at)
    } else {
        None
    }
}

/// Roll over any budget whose window has elapsed (persisting the reset to
/// Postgres so the next usage report accumulates onto zero, not onto the
/// previous window's total), then return every principal's current
/// `Budget` — rolled-over rows included — keyed by principal id.
///
/// One query per elapsed row rather than a single bulk `UPDATE`, matching
/// this module's existing N+1-is-fine rationale (see the module doc
/// comment): rollover checks run once per snapshot rebuild against at most a
/// few dozen budgets, not on the request path.
async fn roll_over_and_load_budgets(pool: &PgPool) -> anyhow::Result<HashMap<i64, Budget>> {
    type BudgetRow = (i64, i64, i64, chrono::DateTime<chrono::Utc>, String);
    let rows: Vec<BudgetRow> = sqlx::query_as(
        "SELECT principal_id, tokens_total, tokens_used, window_start, budget_window FROM budgets",
    )
    .fetch_all(pool)
    .await?;

    let now = chrono::Utc::now();
    let mut budgets = HashMap::with_capacity(rows.len());
    for (principal_id, tokens_total, tokens_used, window_start, window_str) in rows {
        // An unparseable `window` value can only reach here via hand-written
        // SQL bypassing the CHECK constraint — contained to this one row
        // (never rolled over, reported as-is) rather than failing the whole
        // rebuild, the same "one bad row" pattern this module already
        // applies to an undecryptable backend and an unparseable routing
        // rule.
        let Some(window) = BudgetWindow::parse(&window_str) else {
            tracing::error!(
                principal_id,
                window = %window_str,
                "dropping budget rollover: unrecognised window value; the budget is still \
                 published, but will never roll over until the row is corrected"
            );
            budgets.insert(
                principal_id,
                Budget {
                    tokens_total: tokens_total.max(0) as u64,
                    tokens_used: tokens_used.max(0) as u64,
                },
            );
            continue;
        };

        let effective_used = match rolled_over(window_start, window, now) {
            Some(new_start) => {
                sqlx::query(
                    "UPDATE budgets SET tokens_used = 0, window_start = $2, updated_at = now()
                     WHERE principal_id = $1",
                )
                .bind(principal_id)
                .bind(new_start)
                .execute(pool)
                .await?;
                0i64
            }
            None => tokens_used,
        };

        budgets.insert(
            principal_id,
            Budget {
                tokens_total: tokens_total.max(0) as u64,
                tokens_used: effective_used.max(0) as u64,
            },
        );
    }
    Ok(budgets)
}

/// Resolve `virtual_models`/`routing_rules`/`rule_targets`/
/// `virtual_model_defaults` into the pre-evaluated form `crate::routing`
/// consumes.
///
/// Three flat queries rather than one deep join, same rationale as the
/// per-principal permissions query above: this runs once per snapshot
/// rebuild against a handful of rows, and three queries assembled in memory
/// stay far easier to follow than one join across five tables with `weight`
/// and `position` columns that would otherwise need disambiguating aliases.
async fn build_virtual_models(pool: &PgPool) -> anyhow::Result<HashMap<String, VirtualModelDef>> {
    let vm_rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM virtual_models")
        .fetch_all(pool)
        .await?;

    type RuleRow = (i64, i64, serde_json::Value);
    let rule_rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT id, virtual_model_id, match_json FROM routing_rules ORDER BY virtual_model_id, position",
    )
    .fetch_all(pool)
    .await?;

    // `model_id` is resolved to the model's *name* here, once, rather than
    // carried as an id into `crate::routing`: the request path matches
    // targets by the same string it already has (the model name off the
    // wire/body), and routing types staying name-based is what lets
    // `VirtualModelDef::resolve` hand a target straight to
    // `Registry::pool_has_healthy` with no id-to-name lookup on the hot path.
    type TargetRow = (i64, String, i32, i32); // owning id (rule_id or virtual_model_id), model name, weight, position
    let rule_target_rows: Vec<TargetRow> = sqlx::query_as(
        "SELECT rt.rule_id, m.name, rt.weight, rt.position
         FROM rule_targets rt JOIN models m ON m.id = rt.model_id
         ORDER BY rt.rule_id, rt.position",
    )
    .fetch_all(pool)
    .await?;
    let default_target_rows: Vec<TargetRow> = sqlx::query_as(
        "SELECT vd.virtual_model_id, m.name, vd.weight, vd.position
         FROM virtual_model_defaults vd JOIN models m ON m.id = vd.model_id
         ORDER BY vd.virtual_model_id, vd.position",
    )
    .fetch_all(pool)
    .await?;

    let targets_for = |owner_id: i64, rows: &[TargetRow]| -> Vec<WeightedTarget> {
        rows.iter()
            .filter(|(id, ..)| *id == owner_id)
            .map(|(_, model, weight, _)| WeightedTarget {
                model: model.clone(),
                // `weight` is `INT` in Postgres (see the migration) and this
                // schema never writes it negative; a value that somehow is
                // (hand-written SQL, a future bug) is clamped to 0 rather
                // than panicking on the cast — a zero-weight target is
                // simply never chosen by `choose_weighted`, which is a safe
                // degradation for a routing decision to make on its own.
                weight: (*weight).max(0) as u32,
            })
            .collect()
    };

    let mut virtual_models = HashMap::new();
    for (vm_id, vm_name) in vm_rows {
        let rules = rule_rows
            .iter()
            .filter(|(_, virtual_model_id, _)| *virtual_model_id == vm_id)
            .filter_map(|(rule_id, _, match_json)| {
                let parsed: MatchConditionJson = match serde_json::from_value(match_json.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        // Dropped, not defaulted to "matches everyone": a
                        // rule whose condition failed to parse must not
                        // silently become the rule that matches every
                        // request — see the module doc comment on
                        // `build_snapshot` for the same "contain the
                        // failure to the one bad row" pattern applied to an
                        // undecryptable backend.
                        tracing::error!(
                            error = %e,
                            virtual_model = %vm_name,
                            rule_id,
                            "dropping routing rule: match_json failed to parse; excluded from \
                             this snapshot rather than matching every request or failing the \
                             whole rebuild"
                        );
                        return None;
                    }
                };
                Some(RoutingRule {
                    conditions: parsed.into_conditions(),
                    targets: targets_for(*rule_id, &rule_target_rows),
                })
            })
            .collect();
        let default_targets = targets_for(vm_id, &default_target_rows);
        virtual_models.insert(
            vm_name.clone(),
            VirtualModelDef {
                name: vm_name,
                rules,
                default_targets,
            },
        );
    }
    Ok(virtual_models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::test_support::TestCleanup;

    #[test]
    fn a_wildcard_resource_grants_every_model() {
        let perms = vec![("model:invoke".to_string(), "model/*".to_string())];
        let (set, all) = flatten_grants(&perms, &["a".into(), "b".into()]);
        assert!(all);
        assert!(set.is_empty(), "allow_all makes the set redundant");
    }

    #[test]
    fn a_named_resource_grants_only_that_model() {
        let perms = vec![("model:invoke".to_string(), "model/qwen3".to_string())];
        let (set, all) = flatten_grants(&perms, &["qwen3".into(), "other".into()]);
        assert!(!all);
        assert_eq!(set, ["qwen3".to_string()].into_iter().collect());
    }

    #[test]
    fn a_prefix_wildcard_expands_against_the_known_models() {
        let perms = vec![("model:invoke".to_string(), "model/qwen*".to_string())];
        let (set, all) = flatten_grants(&perms, &["qwen3".into(), "qwen2".into(), "llama".into()]);
        assert!(!all);
        assert_eq!(
            set,
            ["qwen3".to_string(), "qwen2".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn permissions_other_than_model_invoke_are_ignored_here() {
        // Admin permissions are not inference permissions and must never leak
        // into the request path's grant set. The resource is deliberately
        // "model/*" (rather than "*") so that if the verb guard were ever
        // removed, this would match the wildcard branch and set allow_all —
        // making the test fail instead of passing vacuously.
        let perms = vec![("config:write".to_string(), "model/*".to_string())];
        let (set, all) = flatten_grants(&perms, &["a".into()]);
        assert!(!all);
        assert!(set.is_empty());
    }

    #[test]
    fn grants_from_several_roles_union() {
        let perms = vec![
            ("model:invoke".to_string(), "model/a".to_string()),
            ("model:invoke".to_string(), "model/b".to_string()),
        ];
        let (set, _) = flatten_grants(&perms, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(
            set,
            ["a".to_string(), "b".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn a_window_still_in_progress_does_not_roll_over() {
        let start = chrono::Utc::now();
        let now = start + chrono::Duration::hours(1);
        assert_eq!(rolled_over(start, BudgetWindow::Daily, now), None);
    }

    #[test]
    fn an_elapsed_window_rolls_over_to_exactly_one_window_forward() {
        let start = chrono::Utc::now() - chrono::Duration::days(2);
        let now = chrono::Utc::now();
        let new_start = rolled_over(start, BudgetWindow::Daily, now)
            .expect("a daily window two days stale must roll over");
        assert_eq!(new_start, start + chrono::Duration::days(1));
    }

    #[test]
    fn budget_window_parses_the_three_known_values_and_rejects_anything_else() {
        assert_eq!(BudgetWindow::parse("daily"), Some(BudgetWindow::Daily));
        assert_eq!(BudgetWindow::parse("weekly"), Some(BudgetWindow::Weekly));
        assert_eq!(BudgetWindow::parse("monthly"), Some(BudgetWindow::Monthly));
        assert_eq!(BudgetWindow::parse("fortnightly"), None);
    }

    fn unique_name(tag: &str) -> String {
        format!(
            "{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Regression for the review finding: one row that fails to decrypt (a
    /// partially completed key rotation, a row never migrated) used to fail
    /// `build_snapshot` outright — a hard exit on the startup path,
    /// crash-looping the control plane and taking every model offline over
    /// one bad backend. It must instead drop just that backend and let the
    /// snapshot build carry every other backend, including a sibling backend
    /// on the very same model.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn one_undecryptable_backend_is_dropped_and_the_rest_of_the_snapshot_still_builds() {
        use crate::control::secrets::test_key;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = test_key();
        let _cleanup = TestCleanup::new()
            .track_prefix("models", "name", "undecryptable-model")
            .track_prefix("models", "name", "unrelated-model");

        let broken_model = unique_name("undecryptable-model");
        let model_id: i64 =
            sqlx::query_scalar("INSERT INTO models (name) VALUES ($1) RETURNING id")
                .bind(&broken_model)
                .fetch_one(&pool)
                .await
                .unwrap();

        // A backend whose `upstream_api_key` cannot possibly decrypt: not
        // `encrypt`-produced ciphertext at all, so `secrets::decrypt` fails
        // on the version byte, the same shape a partially completed key
        // rotation or an unmigrated pre-encryption row would take. Valid
        // UTF-8 (unlike, say, `0xFF` bytes) deliberately: `_cleanup` above
        // removes this row at the end of the test, but this whole module's
        // tests share one scratch database with `control::import`'s while
        // it exists, including `reencrypt_migrates_a_plaintext_row_and_is_idempotent`,
        // which scans *every* `model_backends` row with a non-null
        // `upstream_api_key` and treats "does not decrypt" as "must be a
        // pre-migration plaintext row" (`reencrypt_plaintext_backends`'s
        // documented contract) — bytes that are not valid UTF-8 would trip
        // that function's own refuse-to-guess `bail!` if the two tests ever
        // run concurrently against the same database.
        sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             VALUES ($1, 'http://broken:8000/v1', 'broken', $2)",
        )
        .bind(model_id)
        .bind(b"not-an-encrypt-produced-ciphertext-blob-at-all".to_vec())
        .execute(&pool)
        .await
        .unwrap();

        // A healthy sibling backend on the *same* model, so the test proves
        // containment at backend granularity, not merely "other models
        // survive".
        sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             VALUES ($1, 'http://healthy:8000/v1', 'healthy', NULL)",
        )
        .bind(model_id)
        .execute(&pool)
        .await
        .unwrap();

        // An unrelated model, to prove the failure does not take down the
        // rest of the snapshot either.
        let other_model = unique_name("unrelated-model");
        sqlx::query("INSERT INTO models (name) VALUES ($1)")
            .bind(&other_model)
            .execute(&pool)
            .await
            .unwrap();

        let snapshot = build_snapshot(&pool, &key)
            .await
            .expect("one undecryptable backend must not fail the whole snapshot");

        let model = snapshot
            .models
            .iter()
            .find(|m| m.name == broken_model)
            .expect("the model itself must still be in the snapshot");
        assert_eq!(
            model.backends.len(),
            1,
            "the undecryptable backend must be dropped, the healthy one kept"
        );
        assert_eq!(model.backends[0].api_base, "http://healthy:8000/v1");

        assert!(
            snapshot.models.iter().any(|m| m.name == other_model),
            "an unrelated model must be unaffected"
        );
    }

    /// `build_virtual_models` (via `build_snapshot`) resolves all four P1
    /// tables — `virtual_models`, `routing_rules`, `rule_targets`,
    /// `virtual_model_defaults` — into the pre-evaluated form
    /// `crate::routing` reads. This exercises every one of them together,
    /// against the real schema from `migrations/0008_virtual_models_and_routing_rules.sql`.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn virtual_models_rules_and_targets_are_resolved_into_the_snapshot() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = crate::control::secrets::test_key();
        let _cleanup = TestCleanup::new()
            .track_prefix("models", "name", "vm-primary")
            .track_prefix("models", "name", "vm-secondary")
            .track_prefix("models", "name", "vm-fallback")
            .track_prefix("virtual_models", "name", "vm-canary");

        let primary = unique_name("vm-primary");
        let secondary = unique_name("vm-secondary");
        let fallback = unique_name("vm-fallback");
        for name in [&primary, &secondary, &fallback] {
            sqlx::query("INSERT INTO models (name) VALUES ($1)")
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        let model_id: HashMap<String, i64> = {
            let rows: Vec<(i64, String)> =
                sqlx::query_as("SELECT id, name FROM models WHERE name = ANY($1)")
                    .bind(vec![primary.clone(), secondary.clone(), fallback.clone()])
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            rows.into_iter().map(|(id, name)| (name, id)).collect()
        };

        let vm_name = unique_name("vm-canary");
        let vm_id: i64 =
            sqlx::query_scalar("INSERT INTO virtual_models (name) VALUES ($1) RETURNING id")
                .bind(&vm_name)
                .fetch_one(&pool)
                .await
                .unwrap();

        let match_json = serde_json::json!({ "roles": ["canary"] });
        let rule_id: i64 = sqlx::query_scalar(
            "INSERT INTO routing_rules (virtual_model_id, position, match_json)
             VALUES ($1, 0, $2) RETURNING id",
        )
        .bind(vm_id)
        .bind(&match_json)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO rule_targets (rule_id, model_id, weight, position) VALUES ($1, $2, 70, 0)",
        )
        .bind(rule_id)
        .bind(model_id[&primary])
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO rule_targets (rule_id, model_id, weight, position) VALUES ($1, $2, 30, 1)",
        )
        .bind(rule_id)
        .bind(model_id[&secondary])
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO virtual_model_defaults (virtual_model_id, model_id, weight, position)
             VALUES ($1, $2, 100, 0)",
        )
        .bind(vm_id)
        .bind(model_id[&fallback])
        .execute(&pool)
        .await
        .unwrap();

        let snapshot = build_snapshot(&pool, &key).await.unwrap();
        let vm = snapshot
            .virtual_models
            .get(&vm_name)
            .expect("the virtual model must be in the snapshot");

        assert_eq!(vm.rules.len(), 1);
        let rule = &vm.rules[0];
        assert_eq!(
            rule.conditions.caller.roles,
            ["canary".to_string()].into_iter().collect()
        );
        assert_eq!(
            rule.targets,
            vec![
                crate::routing::WeightedTarget {
                    model: primary.clone(),
                    weight: 70
                },
                crate::routing::WeightedTarget {
                    model: secondary.clone(),
                    weight: 30
                },
            ]
        );
        assert_eq!(
            vm.default_targets,
            vec![crate::routing::WeightedTarget {
                model: fallback.clone(),
                weight: 100
            }]
        );
    }

    /// A principal's role *names*, not just its flattened `allowed_models`,
    /// must reach the snapshot: `crate::routing::CallerMatch` matches rules
    /// by role, and that information has nowhere else to come from on the
    /// request path.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_principals_roles_reach_the_snapshot() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = crate::control::secrets::test_key();
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "roles-reach-snapshot");

        let principal_name = unique_name("roles-reach-snapshot");
        let principal_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&principal_name)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO principal_roles (principal_id, role_id)
             SELECT $1, id FROM roles WHERE name = 'inference'",
        )
        .bind(principal_id)
        .execute(&pool)
        .await
        .unwrap();

        let snapshot = build_snapshot(&pool, &key).await.unwrap();
        let principal = snapshot
            .principals
            .get(&(principal_id as u64))
            .expect("the principal must be in the snapshot");
        assert!(principal.roles.contains("inference"));
    }

    /// A row in `limits` reaches `Principal.limits`, and a principal with no
    /// row is unlimited (`None`), not a bucket with capacity zero.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_configured_limit_reaches_the_snapshot_and_an_unconfigured_principal_is_unlimited() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = crate::control::secrets::test_key();
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "limited-principal")
            .track_prefix("principals", "name", "unlimited-principal");

        let limited_name = unique_name("limited-principal");
        let limited_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&limited_name)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO limits (principal_id, requests_per_min, tokens_per_min)
             VALUES ($1, 42, 4200)",
        )
        .bind(limited_id)
        .execute(&pool)
        .await
        .unwrap();

        let unlimited_name = unique_name("unlimited-principal");
        let unlimited_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&unlimited_name)
        .fetch_one(&pool)
        .await
        .unwrap();

        let snapshot = build_snapshot(&pool, &key).await.unwrap();

        let limited = snapshot
            .principals
            .get(&(limited_id as u64))
            .expect("the limited principal must be in the snapshot");
        assert_eq!(
            limited.limits,
            Some(crate::limiter::Limits {
                requests_per_min: Some(42),
                tokens_per_min: Some(4200),
            })
        );

        let unlimited = snapshot
            .principals
            .get(&(unlimited_id as u64))
            .expect("the unconfigured principal must be in the snapshot too");
        assert_eq!(
            unlimited.limits, None,
            "no row in `limits` must mean unlimited, not a bucket with capacity zero"
        );
    }

    /// Only one of the two dimensions may be set — the schema's own CHECK
    /// constraints allow it (`requests_per_min IS NOT NULL OR tokens_per_min
    /// IS NOT NULL`, each independently nullable), and `build_snapshot` must
    /// carry that through rather than assuming both are always present.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_single_dimension_limit_leaves_the_other_unset() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = crate::control::secrets::test_key();
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "requests-only-principal");

        let name = unique_name("requests-only-principal");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO limits (principal_id, requests_per_min) VALUES ($1, 10)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let snapshot = build_snapshot(&pool, &key).await.unwrap();
        let principal = snapshot.principals.get(&(id as u64)).unwrap();
        assert_eq!(
            principal.limits,
            Some(crate::limiter::Limits {
                requests_per_min: Some(10),
                tokens_per_min: None,
            })
        );
    }

    /// A row in `budgets` reaches `Principal.budget`, and a principal with no
    /// row is unlimited (`None`) — the same shape as the limits test above.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_configured_budget_reaches_the_snapshot_and_an_unconfigured_principal_is_unlimited() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = crate::control::secrets::test_key();
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "budgeted-principal")
            .track_prefix("principals", "name", "unbudgeted-principal");

        let budgeted_name = unique_name("budgeted-principal");
        let budgeted_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&budgeted_name)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO budgets (principal_id, tokens_total, tokens_used, budget_window)
             VALUES ($1, 1000, 250, 'monthly')",
        )
        .bind(budgeted_id)
        .execute(&pool)
        .await
        .unwrap();

        let unbudgeted_name = unique_name("unbudgeted-principal");
        let unbudgeted_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&unbudgeted_name)
        .fetch_one(&pool)
        .await
        .unwrap();

        let snapshot = build_snapshot(&pool, &key).await.unwrap();

        let budgeted = snapshot
            .principals
            .get(&(budgeted_id as u64))
            .expect("the budgeted principal must be in the snapshot");
        assert_eq!(
            budgeted.budget,
            Some(Budget {
                tokens_total: 1000,
                tokens_used: 250,
            })
        );

        let unbudgeted = snapshot
            .principals
            .get(&(unbudgeted_id as u64))
            .expect("the unconfigured principal must be in the snapshot too");
        assert_eq!(
            unbudgeted.budget, None,
            "no row in `budgets` must mean unlimited, not a bucket with capacity zero"
        );
    }

    /// The end-to-end version of `an_elapsed_window_rolls_over_to_exactly_one_window_forward`:
    /// a budget whose window has actually elapsed, sitting in Postgres, comes
    /// back from `build_snapshot` reset to zero — and the row itself is
    /// updated, not just the in-memory value, so the next usage report
    /// accumulates onto zero rather than the stale total.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn an_elapsed_budget_window_is_rolled_over_by_build_snapshot() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = crate::control::secrets::test_key();
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "stale-window-principal");

        let name = unique_name("stale-window-principal");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Truncated to microseconds because that is Postgres `timestamptz`'s
        // resolution: a chrono nanosecond value is rounded on the way in, so
        // comparing what comes back against the un-truncated Rust value can
        // miss by up to 999ns. That made this assertion pass or fail purely
        // on whether the clock's sub-microsecond part happened to be zero —
        // green locally, red in CI.
        use chrono::SubsecRound as _;
        let stale_start = (chrono::Utc::now() - chrono::Duration::days(2)).trunc_subsecs(6);
        sqlx::query(
            "INSERT INTO budgets (principal_id, tokens_total, tokens_used, window_start, budget_window)
             VALUES ($1, 100, 100, $2, 'daily')",
        )
        .bind(id)
        .bind(stale_start)
        .execute(&pool)
        .await
        .unwrap();

        let snapshot = build_snapshot(&pool, &key).await.unwrap();
        let principal = snapshot.principals.get(&(id as u64)).unwrap();
        assert_eq!(
            principal.budget,
            Some(Budget {
                tokens_total: 100,
                tokens_used: 0,
            }),
            "a daily budget two days stale must have rolled over to zero usage"
        );

        let (persisted_used, persisted_start): (i64, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as("SELECT tokens_used, window_start FROM budgets WHERE principal_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            persisted_used, 0,
            "the reset must be persisted, not just returned"
        );
        // Not an exact `stale_start + 1 day`: this test runs against the
        // same live database as `tests/budgets.rs`'s end-to-end test, whose
        // `--role all` process rebuilds (and therefore rolls over every
        // stale budget it finds, this row included) once a second for as
        // long as it runs. Advancing "exactly one window forward per call"
        // is `rolled_over`'s documented behaviour (see its doc comment) —
        // several calls in quick succession legitimately advance the window
        // several times, which an exact equality here would misreport as a
        // bug. What must hold regardless of how many rebuilds raced this
        // one: the window moved forward at least once and landed no later
        // than now.
        assert!(
            persisted_start >= stale_start + chrono::Duration::days(1),
            "the window must have advanced at least one full day"
        );
        assert!(
            persisted_start <= chrono::Utc::now(),
            "rollover must never advance a window into the future"
        );
    }
}
