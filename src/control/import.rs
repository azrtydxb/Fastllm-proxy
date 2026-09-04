//! Seeds the database from a LiteLLM-format config.
//!
//! The migration path off a file-driven deployment, and the reason the
//! LiteLLM-compatibility promise survives the move to a control plane.

use crate::config::{FileConfig, KeyConfig};
use crate::control::api::safe_display_prefix;
use crate::control::secrets::{self, EncryptionKey};
use crate::snapshot::hash_key;
use anyhow::Context;
use sqlx::PgPool;
use std::collections::HashMap;
use std::collections::HashSet;

/// What an import run actually did — every field is a per-run delta, the
/// same way `backends` always was. A prior revision reported `models` as the
/// *total* row count in the table, which read as "N models were just
/// created" on a re-import against a database that already had rows; that
/// was wrong and has been fixed. Re-running the same or an edited file over
/// an already-seeded database should now print zeroes for anything that was
/// already there.
///
/// The `*_existing` counters are how idempotency is signalled rather than
/// merely claimed: a second run of the same file must move every count into
/// its `_existing` column, and a reader who cannot see that has no way to
/// tell "nothing to do" apart from "silently did nothing".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub models: usize,
    pub backends: usize,
    /// Principals created from `auth.keys[].name`.
    pub principals: usize,
    pub principals_existing: usize,
    /// Keys created from `auth.keys[].key`, stored as a SHA-256 hash.
    pub keys: usize,
    pub keys_existing: usize,
    /// `model:invoke` permissions newly attached to a key's own role.
    pub grants: usize,
    pub grants_existing: usize,
    /// Grants removed because the file no longer lists that model. Editing a
    /// key's `models:` down and re-importing has to actually narrow it —
    /// an import that only ever adds would leave a revoked model reachable.
    pub grants_revoked: usize,
}

/// Name of the role that carries one imported key's model grants.
///
/// Per principal, and derived from the principal's name, for two reasons
/// that are both about not surprising an operator later. Derived, so a
/// re-import finds the same role and edits it instead of accumulating a
/// second one every run. Per principal, because `File` mode's grants are
/// per key entry — folding two keys that happen to list the same models
/// into one shared role would mean that narrowing one key's `models:` later
/// silently narrows the other's too.
///
/// The `import:` prefix is what keeps these out of the way of the hand-made
/// roles (`admin`, `operator`, `inference`) an operator manages through
/// `POST /admin/principals/{id}/roles`.
pub fn import_role_name(principal: &str) -> String {
    format!("import:{principal}")
}

/// Seed `providers`/`models` and `auth:` keys from a LiteLLM-format config.
///
/// Idempotent on `(model_id, api_base, upstream_model)` for backends, on
/// `principals.name`, and on `api_keys.hash`: re-running this over the same
/// file, or an edited one, converges the database on the file rather than
/// duplicating it. That is what makes it safe to run against a live
/// deployment's ConfigMap repeatedly while migrating it onto the control
/// plane, rather than requiring one careful one-shot cutover.
///
/// ## How `auth:` becomes RBAC rows
///
/// `File` mode answers "may this key invoke this model?" from
/// `KeyConfig::models` directly (`source::file`); the control plane answers
/// it from `build::flatten_grants` over `permissions` rows. Import has to
/// produce rows that flatten to the same answer, so:
///
/// - `models: ['*']` becomes one `model:invoke` on `model/*`, which
///   `flatten_grants` short-circuits to `allow_all` — the same thing
///   `FileSource` does with a literal `*`.
/// - a named list becomes one `model:invoke` on `model/<name>` per entry.
/// - an absent or empty list grants nothing, matching `FileSource`, where
///   an empty `models` yields an empty `allowed_models` and `allow_all`
///   false. (The README once described omitting it as "every model"; the
///   code has never done that, and import follows the code.)
///
/// Two edges where the two sides genuinely cannot agree, both harmless and
/// both worth knowing about: `flatten_grants` drops a named grant for a
/// model that does not exist (where `FileSource` keeps it — but a grant on a
/// model with no backends is unreachable either way), and it reads a name
/// *ending* in `*` as a prefix wildcard (where `FileSource` reads it
/// literally). Neither is reachable from a config that grants models it
/// actually serves.
///
/// The key's plaintext is hashed with `hash_key` on the way in and is never
/// logged, printed or returned. The file it came from still contains it —
/// that is the operator's copy — but nothing downstream of this function
/// gains a second one.
pub async fn import(
    pool: &PgPool,
    cfg: &FileConfig,
    key: &EncryptionKey,
) -> anyhow::Result<ImportSummary> {
    let mut tx = pool.begin().await?;
    let mut summary = ImportSummary::default();
    let mut models = 0usize;
    let mut backends = 0usize;

    // A `model_name` appearing more than once used to mean one model with
    // several backends. It now means several provider models -- one per
    // endpoint -- held together by a frontend model carrying the name callers
    // already use, which is exactly what migration 0029 produces for the same
    // shape. Counting first is what lets every one of them be qualified, so
    // the bare name is left free for the frontend model.
    let mut occurrences: HashMap<&str, usize> = HashMap::new();
    for entry in &cfg.model_list {
        *occurrences.entry(entry.model_name.as_str()).or_default() += 1;
    }
    // Every provider model that came from a pooled `model_name`, so the
    // frontend model below can point at them.
    let mut pooled: HashMap<String, Vec<String>> = HashMap::new();

    for entry in &cfg.model_list {
        let name = &entry.model_name;
        let api_base = entry.litellm_params.api_base.trim_end_matches('/');
        let upstream = entry.litellm_params.upstream_model(name);
        // upstream_api_key is encrypted at rest with AES-256-GCM
        // (`control::secrets`) before it ever reaches Postgres — see that
        // module's doc comment for exactly what this does and does not
        // protect. `effective_api_key` already strips LiteLLM's
        // `not-needed`/`sk-1234`-style placeholders, so `None` here means
        // "this entry genuinely has no credential", not "encryption was
        // skipped".
        let encrypted_key = entry
            .litellm_params
            .effective_api_key()
            .map(|k| secrets::encrypt(key, &k))
            .transpose()?;
        // The four below are why this is not a bare insert of three columns.
        // `LitellmParams` parses `protocol`, `auth_header`, `auth_scheme` and
        // `default_max_tokens` — they exist so a YAML file can describe an
        // Anthropic or Azure backend — and `Registry::build` honours them on
        // the direct `File`-mode path. Importing dropped all four, so the same
        // file produced an `openai` backend on `Bearer` auth once it reached
        // the database: an Anthropic upstream answering 400s that look like
        // the provider's fault, and an Azure key sent in the wrong header.
        // Nothing warned, because every dropped field had a valid default to
        // fall back to.
        let protocol = entry.litellm_params.protocol_or_default().as_str();
        let auth_header = entry
            .litellm_params
            .auth_header
            .clone()
            .unwrap_or_else(|| "authorization".to_string());
        // Three states, not two: absent means `Bearer`, `""` means send the key
        // raw (NULL in the column), anything else is that prefix. Treating
        // absent and empty alike would quietly drop `Bearer` from every
        // entry that did not mention the field.
        let auth_scheme = entry.litellm_params.auth_scheme_or_default();
        let default_max_tokens = entry.litellm_params.default_max_tokens.map(|t| t as i32);

        // The provider is resolved before the model, because since migration
        // 0029 a model's identity is (provider, name) — the same `model_name`
        // against two `api_base`s is two provider models, which is exactly
        // what a file describing one name on two hosts means.
        //
        // Keyed on the endpoint alone. A provider *is* an endpoint, so
        // `protocol:` or `auth_header:` corrected in the file is a correction
        // to the description of the same provider, not a second one -- keying
        // on them too would fork a new provider on every such edit, which is
        // the opposite of the convergence import exists to give.
        let existing_provider: Option<i64> =
            sqlx::query_scalar("SELECT id FROM providers WHERE api_base = $1")
                .bind(api_base)
                .fetch_optional(&mut *tx)
                .await?;
        let provider_id = match existing_provider {
            // Converge rather than leave the row as first imported: an edited
            // file is the documented way to change policy, so a `protocol:`
            // corrected in YAML that never reached the database would be a
            // silent mismatch from the other direction.
            //
            // The credential is the one field written only when the file names
            // one. A file with no `api_key` usually means it was set through
            // the admin API afterwards, and overwriting it with NULL on the
            // next import would revoke a working provider for nothing.
            Some(id) => {
                sqlx::query(
                    "UPDATE providers SET protocol = $2, auth_header = $3, auth_scheme = $4, \
                     upstream_api_key = COALESCE($5, upstream_api_key) WHERE id = $1",
                )
                .bind(id)
                .bind(protocol)
                .bind(&auth_header)
                .bind(&auth_scheme)
                .bind(&encrypted_key)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                let host = api_base
                    .split_once("://")
                    .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
                    .unwrap_or(api_base);
                let mut provider_name = host.to_string();
                for n in 2..100 {
                    let taken: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM providers WHERE name = $1)",
                    )
                    .bind(&provider_name)
                    .fetch_one(&mut *tx)
                    .await?;
                    if !taken {
                        break;
                    }
                    provider_name = format!("{host}#{n}");
                }
                let id: i64 = sqlx::query_scalar(
                    "INSERT INTO providers (name, api_base, protocol, auth_header, auth_scheme, \
                     upstream_api_key) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
                )
                .bind(&provider_name)
                .bind(api_base)
                .bind(protocol)
                .bind(&auth_header)
                .bind(&auth_scheme)
                .bind(&encrypted_key)
                .fetch_one(&mut *tx)
                .await?;
                backends += 1;
                id
            }
        };

        // A file naming one `model_name` against two `api_base`s means two
        // provider models. Model names are still globally unique until
        // addressing moves to the frontend model (ADR 0005), so the second one
        // is qualified by its provider -- the same shape migration 0029
        // produces, so an imported database and a migrated one look alike.
        let provider_name: String = sqlx::query_scalar("SELECT name FROM providers WHERE id = $1")
            .bind(provider_id)
            .fetch_one(&mut *tx)
            .await?;
        let qualified = format!("{name}@{provider_name}");

        // Look up before inserting (rather than INSERT ... ON CONFLICT and
        // checking `xmax`) so a genuine per-run count of *new* models falls
        // out directly — no separate "count what changed" query needed.
        // Either spelling counts as this provider's, so a re-import converges
        // onto the row it created last time rather than adding a third.
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM models WHERE provider_id = $1 AND name IN ($2, $3)")
                .bind(provider_id)
                .bind(name)
                .bind(&qualified)
                .fetch_optional(&mut *tx)
                .await?;
        let pooled_here = occurrences.get(name.as_str()).copied().unwrap_or(1) > 1;
        let insert_name = if pooled_here {
            // Every one of them, not just the second: the bare name has to be
            // free for the frontend model that will carry it.
            qualified.clone()
        } else {
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM models WHERE name = $1 AND provider_id <> $2)",
            )
            .bind(name)
            .bind(provider_id)
            .fetch_one(&mut *tx)
            .await?;
            if taken {
                qualified.clone()
            } else {
                name.clone()
            }
        };
        if pooled_here {
            pooled
                .entry(name.clone())
                .or_default()
                .push(insert_name.clone());
        }
        match existing {
            // Converge rather than leave the row as first imported. An edited
            // file is the documented way to change policy, so an
            // `upstream_model` corrected in YAML that never reached the
            // database would be a silent mismatch from the other direction.
            Some(id) => {
                sqlx::query(
                    "UPDATE models SET name = $2, upstream_model = $3, default_max_tokens = $4 \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(&insert_name)
                .bind(&upstream)
                .bind(default_max_tokens)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO models (name, provider_id, upstream_model, default_max_tokens) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(&insert_name)
                .bind(provider_id)
                .bind(&upstream)
                .bind(default_max_tokens)
                .execute(&mut *tx)
                .await?;
                models += 1;
            }
        }
    }

    // The frontend model that keeps a pooled `model_name` callable. Without
    // it, a config meaning "one name, two hosts" would leave callers naming
    // something that no longer exists.
    for (client_name, targets) in &pooled {
        let vm_id: i64 = sqlx::query_scalar(
            "INSERT INTO virtual_models (name, description) VALUES ($1, $2) \
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
        )
        .bind(client_name)
        .bind(format!(
            "Balances {client_name} across the providers the imported config names for it."
        ))
        .fetch_one(&mut *tx)
        .await?;
        // Rebuilt rather than merged, so an edited file that drops a host
        // drops the target too instead of routing there forever.
        sqlx::query("DELETE FROM virtual_model_defaults WHERE virtual_model_id = $1")
            .bind(vm_id)
            .execute(&mut *tx)
            .await?;
        for (position, target) in targets.iter().enumerate() {
            // Equal weight: the file expressed no preference between the
            // hosts, and inventing one would be a change of behaviour
            // disguised as an import.
            sqlx::query(
                "INSERT INTO virtual_model_defaults (virtual_model_id, model_id, weight, position) \
                 SELECT $1, id, 1, $3 FROM models WHERE name = $2",
            )
            .bind(vm_id)
            .bind(target)
            .bind(position as i32)
            .execute(&mut *tx)
            .await?;
        }
    }

    summary.models = models;
    summary.backends = backends;

    // Keys after models, in the same transaction: a named grant only
    // flattens into the snapshot if the model exists (see `flatten_grants`),
    // so importing grants against models this same run is creating has to
    // see them.
    for entry in &cfg.auth.keys {
        import_key(&mut tx, entry, &mut summary).await?;
    }

    tx.commit().await?;
    Ok(summary)
}

/// One `auth.keys` entry: its principal, its own role, that role's grants,
/// and the key itself.
async fn import_key(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry: &KeyConfig,
    summary: &mut ImportSummary,
) -> anyhow::Result<()> {
    let principal_name = entry.name.trim();
    if principal_name.is_empty() {
        anyhow::bail!("an auth.keys entry has an empty name; a name is what its principal and its grant role are keyed on");
    }

    // `ON CONFLICT DO NOTHING RETURNING id` returns nothing for a row that
    // already existed, which is exactly the "was it new?" signal the summary
    // needs — no second query to count what changed. `imported` is set TRUE
    // only here, on the row `import` itself creates — see migration 0007's
    // comment for why that is what lets the `None` branch below tell an
    // earlier import run's principal apart from one a human or the admin API
    // created that only happens to share the name.
    let created: Option<i64> = sqlx::query_scalar(
        "INSERT INTO principals (kind, name, imported) VALUES ('service_account', $1, TRUE)
         ON CONFLICT (name) DO NOTHING RETURNING id",
    )
    .bind(principal_name)
    .fetch_optional(&mut **tx)
    .await?;
    let principal_id = match created {
        Some(id) => {
            summary.principals += 1;
            id
        }
        None => {
            // A principal with this name already exists. `import` may only
            // attach to it if `import` itself was what created it —
            // otherwise this is the review finding this branch exists to
            // fix: a config naming an existing, privileged, hand-created
            // principal would otherwise mint a key that inherits every role
            // already granted to that principal, with nothing about the
            // config file itself asking for that.
            let (existing_id, imported): (i64, bool) =
                sqlx::query_as("SELECT id, imported FROM principals WHERE name = $1")
                    .bind(principal_name)
                    .fetch_one(&mut **tx)
                    .await?;
            if !imported {
                anyhow::bail!(
                    "auth.keys entry {principal_name:?} names an existing principal (id \
                     {existing_id}) that `import` did not create. Refusing to attach a new key \
                     to it: doing so would silently grant that key every role already granted to \
                     the existing principal. Rename this auth.keys entry, or first rename or \
                     delete the existing principal through the admin API if the two are meant to \
                     be the same."
                );
            }
            summary.principals_existing += 1;
            existing_id
        }
    };

    let role_name = import_role_name(principal_name);
    let role_id: i64 = match sqlx::query_scalar(
        "INSERT INTO roles (name, description) VALUES ($1, $2)
         ON CONFLICT (name) DO NOTHING RETURNING id",
    )
    .bind(&role_name)
    .bind(format!(
        "Model grants imported from the auth: block for {principal_name}"
    ))
    .fetch_optional(&mut **tx)
    .await?
    {
        Some(id) => id,
        None => {
            sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
                .bind(&role_name)
                .fetch_one(&mut **tx)
                .await?
        }
    };

    sqlx::query(
        "INSERT INTO principal_roles (principal_id, role_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(principal_id)
    .bind(role_id)
    .execute(&mut **tx)
    .await?;

    // See this module's doc comment for why `*` maps to `model/*` rather
    // than to the expanded model list: the wildcard has to stay a wildcard,
    // so a model added tomorrow is covered without a re-import.
    let wanted: HashSet<String> = if entry.models.iter().any(|m| m == "*") {
        ["model/*".to_string()].into_iter().collect()
    } else {
        entry
            .models
            .iter()
            .map(|m| format!("model/{m}"))
            .collect::<HashSet<_>>()
    };

    for resource in &wanted {
        let permission_id: i64 = match sqlx::query_scalar(
            "INSERT INTO permissions (verb, resource) VALUES ('model:invoke', $1)
             ON CONFLICT (verb, resource) DO NOTHING RETURNING id",
        )
        .bind(resource)
        .fetch_optional(&mut **tx)
        .await?
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "SELECT id FROM permissions WHERE verb = 'model:invoke' AND resource = $1",
                )
                .bind(resource)
                .fetch_one(&mut **tx)
                .await?
            }
        };
        let granted = sqlx::query(
            "INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(role_id)
        .bind(permission_id)
        .execute(&mut **tx)
        .await?;
        if granted.rows_affected() == 0 {
            summary.grants_existing += 1;
        } else {
            summary.grants += 1;
        }
    }

    // Narrowing a key's `models:` in the file and re-importing has to
    // actually narrow it. Only this key's own role is touched — that is the
    // whole reason the role is per principal.
    let wanted_list: Vec<String> = wanted.into_iter().collect();
    let revoked = sqlx::query(
        "DELETE FROM role_permissions rp USING permissions p
         WHERE rp.permission_id = p.id AND rp.role_id = $1
           AND NOT (p.verb = 'model:invoke' AND p.resource = ANY($2))",
    )
    .bind(role_id)
    .bind(&wanted_list)
    .execute(&mut **tx)
    .await?;
    summary.grants_revoked += revoked.rows_affected() as usize;

    let expires_at = entry
        .expires_at
        .as_deref()
        .map(parse_expiry)
        .transpose()
        .with_context(|| format!("auth.keys entry {principal_name:?}: bad expires_at"))?;

    // Looked up by hash, then updated, rather than `ON CONFLICT (hash) DO
    // UPDATE`: the update fires either way there, so the summary could not
    // tell a new key from a re-imported one. The update itself matters —
    // an edited `expires_at` in the file must land.
    //
    // `disabled = FALSE` is unconditional and deliberate, reconsidered as
    // part of the same review that added the `imported` check above: a key
    // is looked up here by its *hash*, i.e. by its exact plaintext value, so
    // the only way this branch re-enables a disabled key is an operator
    // running `import` again over a file that still names that exact key.
    // `import`'s whole contract is "converge the database on the file" (see
    // this function's doc comment — narrowing `models:` and re-importing
    // already revokes grants the same way), and a config file is the
    // operator's own source of truth: if it still lists the key, that is the
    // operator asking for it to be live, the same as adding a brand new key
    // would be. The one-time, out-of-band revocation this could plausibly
    // surprise — `DELETE`ing a key through the admin API without also
    // editing the file — is exactly what `deploy/README.md` documents `POST
    // /admin/keys/{id}` (not editing the ConfigMap) for; a key an operator
    // truly wants gone belongs out of the file, not merely disabled while
    // import keeps re-seeding it.
    let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM api_keys WHERE hash = $1")
        .bind(hash_key(&entry.key).to_vec())
        .fetch_optional(&mut **tx)
        .await?;
    match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE api_keys SET name = $1, principal_id = $2, expires_at = $3,
                        disabled = FALSE
                 WHERE id = $4",
            )
            .bind(principal_name)
            .bind(principal_id)
            .bind(expires_at)
            .bind(id)
            .execute(&mut **tx)
            .await?;
            summary.keys_existing += 1;
        }
        None => {
            sqlx::query(
                "INSERT INTO api_keys (hash, prefix, name, principal_id, expires_at)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(hash_key(&entry.key).to_vec())
            .bind(safe_display_prefix(&entry.key))
            .bind(principal_name)
            .bind(principal_id)
            .bind(expires_at)
            .execute(&mut **tx)
            .await?;
            summary.keys += 1;
        }
    }

    // `limits:` and `budget:` are part of the `auth:` block and were being
    // dropped, the same way the backend's protocol and auth columns were: they
    // parse, they validate, and nothing wrote them. A key imported out of a
    // rate-limited `File`-mode deployment arrived in the database unlimited,
    // which is the failure direction that matters.
    if let Some(limits) = entry.limits {
        sqlx::query(
            "INSERT INTO limits (principal_id, requests_per_min, tokens_per_min)
             VALUES ($1, $2, $3)
             ON CONFLICT (principal_id) DO UPDATE
               SET requests_per_min = EXCLUDED.requests_per_min,
                   tokens_per_min = EXCLUDED.tokens_per_min,
                   updated_at = now()",
        )
        .bind(principal_id)
        .bind(limits.requests_per_min.map(|v| v as i32))
        .bind(limits.tokens_per_min.map(|v| v as i32))
        .execute(&mut **tx)
        .await?;
    }

    if let Some(budget) = entry.budget {
        // `monthly` because the file format has no window at all: `File` mode
        // has no reconciliation loop, so its `tokens_used` is a static
        // starting point rather than a rolling count. A window has to be
        // chosen to reach a NOT NULL column, and the longest one is the choice
        // least likely to refuse traffic the file never meant to refuse.
        //
        // `tokens_used` is written on insert and never on update. Once this
        // budget is in the database it advances from real usage, and letting a
        // static number in a config file rewind it on the next import would
        // hand back spend that was already consumed.
        sqlx::query(
            "INSERT INTO budgets (principal_id, tokens_total, tokens_used, budget_window)
             VALUES ($1, $2, $3, 'monthly')
             ON CONFLICT (principal_id) DO UPDATE
               SET tokens_total = EXCLUDED.tokens_total,
                   updated_at = now()",
        )
        .bind(principal_id)
        .bind(budget.tokens_total as i64)
        .bind(budget.tokens_used as i64)
        .execute(&mut **tx)
        .await?;
    }

    Ok(())
}

/// Parsed with the same `humantime::parse_rfc3339` `FileSource` uses, not
/// with chrono's looser RFC 3339 reader, so a timestamp the file-mode proxy
/// would reject cannot be quietly accepted here — the two modes have to
/// agree on which files are valid, not just on what valid ones mean.
fn parse_expiry(s: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    Ok(humantime::parse_rfc3339(s)?.into())
}

/// One-shot migration for rows written before encryption-at-rest existed
/// (`migrations/0004_encrypted_upstream_api_key.sql`).
///
/// Chosen over a format that silently distinguishes plaintext from
/// ciphertext on every read: `build_snapshot` and `import` both need
/// `upstream_api_key` to mean one thing, "an `encrypt`-produced blob", on
/// every code path, permanently — teaching the hot read path to also
/// tolerate raw plaintext bytes forever, for the sake of rows that exist on
/// at most a handful of developer scratch databases (the live cluster has
/// no control-plane database yet at all), is a worse trade than a command
/// an operator runs once after upgrading.
///
/// Detection still uses the self-describing format, just here instead of on
/// every read: a row is left alone if it already decrypts under `key` (see
/// `secrets::is_encrypted` for why that is a safe classifier), and
/// re-encrypted in place otherwise, on the assumption that anything which
/// doesn't decrypt is the raw plaintext bytes `import` used to write
/// directly. Safe to run more than once — an already-migrated database is a
/// no-op — and safe to run against a database with no plaintext rows at all.
///
/// Generic over `sqlx::Acquire` (both `&PgPool` and `&mut Transaction` work)
/// rather than pinned to `&PgPool`, purely so a test can drive it inside one
/// transaction and prove the migration is atomic — `build_snapshot` decrypts
/// every non-null `upstream_api_key` row it sees, so any window in which a
/// pre-migration plaintext row is visible on the shared connection pool but
/// not yet migrated is a window in which an unrelated `build_snapshot` call
/// against the same database would fail. Every production call site still
/// passes `&PgPool` exactly as before.
pub async fn reencrypt_plaintext_backends<'a, A>(
    conn: A,
    key: &EncryptionKey,
) -> anyhow::Result<usize>
where
    A: sqlx::Acquire<'a, Database = sqlx::Postgres>,
{
    reencrypt_plaintext_backends_scoped(conn, key, None).await
}

/// The same sweep, optionally narrowed to one model's provider.
///
/// The whole-table form is what an operator wants for a key rotation, and is
/// right to refuse the batch when it meets a blob it can neither decrypt nor
/// read as plaintext — guessing at a credential is the one thing it must not
/// do. But that same property makes the unscoped form untestable against a
/// database that holds *real* rows encrypted under a *different* key, which is
/// exactly what this repo's shared dev Postgres now contains. Scoping is how a
/// test says "these rows are mine"; it is not a weaker check, it is a smaller
/// one.
pub async fn reencrypt_plaintext_backends_scoped<'a, A>(
    conn: A,
    key: &EncryptionKey,
    only_model_id: Option<i64>,
) -> anyhow::Result<usize>
where
    A: sqlx::Acquire<'a, Database = sqlx::Postgres>,
{
    let mut conn = conn.acquire().await?;
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT id, upstream_api_key FROM providers
         WHERE upstream_api_key IS NOT NULL
           AND ($1::bigint IS NULL
                OR id = (SELECT provider_id FROM models WHERE id = $1))",
    )
    .bind(only_model_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut migrated = 0usize;
    for (id, blob) in rows {
        if secrets::is_encrypted(key, &blob) {
            continue;
        }
        let plaintext = String::from_utf8(blob).map_err(|_| {
            anyhow::anyhow!(
                "providers.id={id}: upstream_api_key is neither valid ciphertext under the \
                 given key nor valid UTF-8 plaintext; refusing to guess"
            )
        })?;
        let reencrypted = secrets::encrypt(key, &plaintext)?;
        sqlx::query("UPDATE providers SET upstream_api_key = $1 WHERE id = $2")
            .bind(reencrypted)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        migrated += 1;
    }
    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch database used for these tests is shared across the whole
    /// `cargo test` invocation (and, in practice, with whatever else happens
    /// to be pointed at it). The brief's original tests started with
    /// `TRUNCATE models, providers CASCADE`, which is fine in isolation
    /// but races with any other test truncating the same tables concurrently
    /// — including each other, once `models` stopped being a total count and
    /// started being a per-run delta (a truncate from the sibling test
    /// landing between this test's two `import` calls made the second call
    /// see an empty table and report a spurious "new" model). Uniquely-named
    /// models sidestep the whole class of race instead of trying to order
    /// two `#[tokio::test]`s that `cargo test` is free to run in parallel.
    fn unique_name(tag: &str) -> String {
        format!(
            "{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    use crate::control::test_support::TestCleanup;
    /// Shared with `control::api::tests` — see `secrets::test_key` for why
    /// every DB-backed test in `control::*` must use the same key rather
    /// than each picking its own.
    use secrets::test_key;

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_the_deployed_config_creates_models_and_backends() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        // Not `unique_name("qwen3")` — the tag itself would prefix-match
        // the real registered model `qwen3-6-35b-a3b-nvfp4`, which this
        // suite must never touch. `qwen3-import-test` is unambiguous.
        let _cleanup = TestCleanup::new()
            .track_prefix("models", "name", "qwen3-import-test-")
            .track_prefix("virtual_models", "name", "qwen3-import-test-");

        let name = unique_name("qwen3-import-test");
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n\
             \x20 - model_name: {name}\n\
             \x20   litellm_params: {{ model: openai/{name}, api_base: http://{name}-a:8000/v1 }}\n\
             \x20 - model_name: {name}\n\
             \x20   litellm_params: {{ model: openai/{name}, api_base: http://{name}-b:8000/v1 }}\n"
        ))
        .unwrap();

        let summary = import(&pool, &cfg, &test_key()).await.unwrap();
        // Two entries sharing a model_name are two provider models on two
        // providers, because a provider model has exactly one provider since
        // migration 0029. They used to be one model with two backends.
        assert_eq!(summary.models, 2);
        assert_eq!(summary.backends, 2, "one provider per endpoint");

        // And callers naming the pooled name still reach something: the
        // frontend model holds it and balances across both, the same shape
        // migration 0029 produces for a model that had two backends.
        let targets: Vec<String> = sqlx::query_scalar(
            "SELECT m.name FROM virtual_models v \
             JOIN virtual_model_defaults d ON d.virtual_model_id = v.id \
             JOIN models m ON m.id = d.model_id \
             WHERE v.name = $1 ORDER BY d.position",
        )
        .bind(&name)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            targets,
            vec![format!("{name}@a:8000"), format!("{name}@b:8000")],
            "the pooled name must reach both providers"
        );

        // Re-running must converge, not accumulate a third target.
        let again = import(&pool, &cfg, &test_key()).await.unwrap();
        assert_eq!(again.models, 0, "re-import must report no new model");
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM virtual_model_defaults d \
             JOIN virtual_models v ON v.id = d.virtual_model_id WHERE v.name = $1",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2, "re-import must be idempotent");
    }

    /// Every backend column a YAML file can name, checked against the row.
    ///
    /// This is the test that was missing. `import` wrote three columns and
    /// silently dropped `protocol`, `auth_header`, `auth_scheme` and
    /// `default_max_tokens`, so an Anthropic backend imported as an OpenAI one
    /// and an Azure key went out in the wrong header — and every existing test
    /// passed, because each dropped field has a valid default and the counts
    /// were still right.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn import_carries_every_backend_field_the_file_can_name() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let _cleanup = TestCleanup::new().track_prefix("models", "name", "native-import-test-");

        let name = unique_name("native-import-test");
        // The endpoint is derived from the unique name, not the real
        // `api.anthropic.com`. A provider is keyed on its endpoint and shared
        // by every model on it since migration 0029, so importing the real
        // hostname here would converge onto the deployment's own Anthropic
        // provider and overwrite its credential with `sk-ant-test`. This suite
        // runs against a shared database.
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n\
             \x20 - model_name: {name}\n\
             \x20   litellm_params:\n\
             \x20     model: anthropic/claude-sonnet-4\n\
             \x20     api_base: https://{name}.example.invalid/v1\n\
             \x20     api_key: sk-ant-test\n\
             \x20     protocol: anthropic\n\
             \x20     auth_header: x-api-key\n\
             \x20     auth_scheme: \"\"\n\
             \x20     default_max_tokens: 4096\n"
        ))
        .unwrap();

        import(&pool, &cfg, &test_key()).await.unwrap();

        let (protocol, auth_header, auth_scheme, max_tokens): (
            String,
            String,
            Option<String>,
            Option<i32>,
        ) = sqlx::query_as(
            "SELECT p.protocol, p.auth_header, p.auth_scheme, m.default_max_tokens
               FROM models m JOIN providers p ON p.id = m.provider_id
              WHERE m.name = $1",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(protocol, "anthropic", "protocol was dropped on import");
        assert_eq!(auth_header, "x-api-key", "auth_header was dropped");
        // `auth_scheme: ""` means send the key raw. As an empty string it
        // would be prepended to the key with a space; NULL is how the column
        // spells the same intent.
        assert_eq!(auth_scheme, None, "empty auth_scheme must store as NULL");
        assert_eq!(max_tokens, Some(4096), "default_max_tokens was dropped");
    }

    /// The `auth:` block's enforcement, not just its identity.
    ///
    /// A key imported out of a rate-limited `File`-mode deployment used to
    /// arrive unlimited: `limits` and `budget` parse and validate, and nothing
    /// wrote them. That is the failure direction that matters — the key works,
    /// so nobody looks, and the cap silently is not there.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn import_carries_the_limits_and_budget_on_a_key() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        // `import` creates a role per key (`import:<name>`) as well as the
        // principal, and tracking only the principal left fifteen of them
        // behind in the shared database before anyone noticed.
        let _cleanup = TestCleanup::new()
            .track_prefix("models", "name", "limits-import-test-")
            .track_prefix("principals", "name", "limits-sa-")
            .track_prefix("roles", "name", "import:limits-sa-");

        let model = unique_name("limits-import-test");
        let principal = unique_name("limits-sa");
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n\
             \x20 - model_name: {model}\n\
             \x20   litellm_params: {{ api_base: http://{name}-h:8000/v1 }}\n\
             auth:\n\
             \x20 keys:\n\
             \x20   - key: sk-limits-import-test-aaaaaaaaaaaa\n\
             \x20     name: {principal}\n\
             \x20     models: [{model}]\n\
             \x20     limits:\n\
             \x20       requests_per_min: 60\n\
             \x20       tokens_per_min: 100000\n\
             \x20     budget:\n\
             \x20       tokens_total: 1000000\n"
        ))
        .unwrap();

        import(&pool, &cfg, &test_key()).await.unwrap();

        let (rpm, tpm): (Option<i32>, Option<i32>) = sqlx::query_as(
            "SELECT l.requests_per_min, l.tokens_per_min
               FROM limits l JOIN principals p ON p.id = l.principal_id
              WHERE p.name = $1",
        )
        .bind(&principal)
        .fetch_one(&pool)
        .await
        .expect("a limits row for the imported principal");
        assert_eq!(rpm, Some(60), "requests_per_min was dropped on import");
        assert_eq!(tpm, Some(100_000), "tokens_per_min was dropped on import");

        let (total, window): (Option<i64>, String) = sqlx::query_as(
            "SELECT b.tokens_total, b.budget_window
               FROM budgets b JOIN principals p ON p.id = b.principal_id
              WHERE p.name = $1",
        )
        .bind(&principal)
        .fetch_one(&pool)
        .await
        .expect("a budgets row for the imported principal");
        assert_eq!(total, Some(1_000_000), "the budget was dropped on import");
        assert_eq!(window, "monthly");

        // Consumption is live state. A static number in a config file must not
        // hand back spend that has already happened.
        sqlx::query(
            "UPDATE budgets SET tokens_used = 500000
               WHERE principal_id = (SELECT id FROM principals WHERE name = $1)",
        )
        .bind(&principal)
        .execute(&pool)
        .await
        .unwrap();
        import(&pool, &cfg, &test_key()).await.unwrap();
        let used: i64 = sqlx::query_scalar(
            "SELECT b.tokens_used FROM budgets b JOIN principals p ON p.id = b.principal_id
              WHERE p.name = $1",
        )
        .bind(&principal)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(used, 500_000, "re-importing must not rewind consumption");
    }

    /// An edited file has to reach the database, not just avoid duplicating.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn re_importing_an_edited_backend_converges() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let _cleanup = TestCleanup::new().track_prefix("models", "name", "converge-import-test-");

        let name = unique_name("converge-import-test");
        let yaml = |protocol: &str, tokens: &str| {
            format!(
                "model_list:\n\
                 \x20 - model_name: {name}\n\
                 \x20   litellm_params:\n\
                 \x20     api_base: https://{name}.example.invalid/v1\n\
                 \x20     api_key: sk-ant-test\n\
                 \x20     protocol: {protocol}\n\
                 \x20     default_max_tokens: {tokens}\n"
            )
        };

        // Imported wrong first, the way a typo reaches production.
        let first: crate::config::FileConfig =
            serde_yaml::from_str(&yaml("openai", "1024")).unwrap();
        import(&pool, &first, &test_key()).await.unwrap();

        let second: crate::config::FileConfig =
            serde_yaml::from_str(&yaml("anthropic", "4096")).unwrap();
        let summary = import(&pool, &second, &test_key()).await.unwrap();
        assert_eq!(summary.backends, 0, "a corrected file must not add a row");

        let (protocol, max_tokens): (String, Option<i32>) = sqlx::query_as(
            "SELECT p.protocol, m.default_max_tokens
               FROM models m JOIN providers p ON p.id = m.provider_id
              WHERE m.name = $1",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            protocol, "anthropic",
            "the correction never reached the row"
        );
        assert_eq!(max_tokens, Some(4096));
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_twice_does_not_duplicate() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        // `"m"` alone would be too broad a prefix to track safely (it would
        // match any other test's/run's model name that happens to start
        // with the same letter); a distinctive tag makes tracking exact.
        let _cleanup = TestCleanup::new().track_prefix("models", "name", "import-dup-test-");

        let name = unique_name("import-dup-test");
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n  - model_name: {name}\n    litellm_params: {{ api_base: http://{name}-a:8000/v1 }}\n"
        ))
        .unwrap();
        let first = import(&pool, &cfg, &test_key()).await.unwrap();
        assert_eq!(first.models, 1, "first run creates the one new model");
        let second = import(&pool, &cfg, &test_key()).await.unwrap();
        // `models` is a per-run delta, same as `backends`: nothing new was
        // created on the second run, so it reports zero, not the table total.
        assert_eq!(second.models, 0, "re-import must not report a new model");

        let model_id: i64 = sqlx::query_scalar("SELECT id FROM models WHERE name = $1")
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM models WHERE id = $1 AND provider_id IS NOT NULL",
        )
        .bind(model_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "re-import must be idempotent");
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn the_stored_column_is_not_the_plaintext_credential_and_build_snapshot_recovers_it() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = test_key();
        let _cleanup = TestCleanup::new().track_prefix("models", "name", "secret-backend-");

        let name = unique_name("secret-backend");
        let plaintext_credential = "sk-do-not-leak-this-upstream-token";
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "model_list:\n  - model_name: {name}\n    litellm_params: {{ api_base: http://{name}-a:8000/v1, api_key: {plaintext_credential} }}\n"
        ))
        .unwrap();

        import(&pool, &cfg, &key).await.unwrap();

        let stored: Vec<u8> = sqlx::query_scalar(
            "SELECT p.upstream_api_key FROM models m
             JOIN providers p ON p.id = m.provider_id
             WHERE m.name = $1",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();

        // What's in the column must not be the plaintext credential...
        assert_ne!(stored, plaintext_credential.as_bytes());
        assert!(String::from_utf8_lossy(&stored)
            .find(plaintext_credential)
            .is_none());
        // ...and must not be readable without the key.
        let wrong_key = secrets::EncryptionKey::from_hex(&"cd".repeat(secrets::KEY_LEN)).unwrap();
        assert!(secrets::decrypt(&wrong_key, &stored).is_err());

        // The right key recovers it, both directly...
        assert_eq!(
            secrets::decrypt(&key, &stored).unwrap(),
            plaintext_credential
        );
        // ...and through the path the proxy actually uses.
        let snapshot = crate::control::build::build_snapshot(&pool, &key)
            .await
            .unwrap();
        let model = snapshot.models.iter().find(|m| m.name == name).unwrap();
        assert_eq!(
            model.backends[0].api_key.as_deref(),
            Some(plaintext_credential)
        );
    }

    /// A config with an `auth:` block, in both `File` mode and control-plane
    /// form, so one test body can drive both sides of the comparison.
    struct AuthFixture {
        yaml: String,
        granted_model: String,
        ungranted_model: String,
        named_key: String,
        named_principal: String,
        wildcard_key: String,
        wildcard_principal: String,
    }

    fn auth_fixture() -> AuthFixture {
        let granted_model = unique_name("granted");
        let ungranted_model = unique_name("ungranted");
        let named_key = unique_name("sk-named-key-long-enough-to-prefix");
        let named_principal = unique_name("named-principal");
        let wildcard_key = unique_name("sk-wildcard-key-long-enough-to-prefix");
        let wildcard_principal = unique_name("wildcard-principal");
        let yaml = format!(
            "model_list:\n\
             \x20 - model_name: {granted_model}\n\
             \x20   litellm_params: {{ api_base: http://{granted_model}:8000/v1 }}\n\
             \x20 - model_name: {ungranted_model}\n\
             \x20   litellm_params: {{ api_base: http://{ungranted_model}:8000/v1 }}\n\
             auth:\n\
             \x20 keys:\n\
             \x20   - key: {named_key}\n\
             \x20     name: {named_principal}\n\
             \x20     models: [{granted_model}]\n\
             \x20     expires_at: \"2035-01-01T00:00:00Z\"\n\
             \x20   - key: {wildcard_key}\n\
             \x20     name: {wildcard_principal}\n\
             \x20     models: ['*']\n"
        );
        AuthFixture {
            yaml,
            granted_model,
            ungranted_model,
            named_key,
            named_principal,
            wildcard_key,
            wildcard_principal,
        }
    }

    impl AuthFixture {
        /// Every row `import` creates from this fixture: the two models,
        /// the two principals, and (unlike a principal's other roles, which
        /// cascade away with it) the two `import:<principal>` roles
        /// `import_key` creates alongside each principal.
        fn cleanup(&self) -> TestCleanup {
            TestCleanup::new()
                .track_exact("models", "name", self.granted_model.clone())
                .track_exact("models", "name", self.ungranted_model.clone())
                .track_exact("principals", "name", self.named_principal.clone())
                .track_exact("principals", "name", self.wildcard_principal.clone())
                .track_exact("roles", "name", import_role_name(&self.named_principal))
                .track_exact("roles", "name", import_role_name(&self.wildcard_principal))
        }
    }

    /// The point of importing `auth:` at all: a file that authorised a set of
    /// calls in `File` mode must authorise exactly the same set once it has
    /// been imported and the control plane has built a snapshot from the
    /// database. Anything less makes `import` a migration that silently
    /// changes who can reach what.
    ///
    /// Compared through `authenticate`/`may_invoke` rather than by diffing
    /// rows, because those two calls *are* the request path's questions —
    /// principal ids legitimately differ between the two sources (`File` mode
    /// numbers them by position, Postgres by sequence) and a row-level
    /// comparison would fail on that difference while missing a real one.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn imported_auth_authorises_exactly_what_file_mode_authorises() {
        use crate::source::SnapshotSource;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let fx = auth_fixture();
        let _cleanup = fx.cleanup();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, fx.yaml.as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let file_snapshot = crate::source::file::FileSource::new(file.path().into())
            .fetch(None)
            .await
            .unwrap()
            .unwrap();

        let cfg: crate::config::FileConfig = serde_yaml::from_str(&fx.yaml).unwrap();
        let summary = import(&pool, &cfg, &test_key()).await.unwrap();
        assert_eq!(summary.principals, 2, "one principal per auth.keys entry");
        assert_eq!(summary.keys, 2);
        assert_eq!(
            summary.grants, 2,
            "one model:invoke grant for the named model, one for the wildcard"
        );

        let db_snapshot = crate::control::build::build_snapshot(&pool, &test_key())
            .await
            .unwrap();

        let now = std::time::SystemTime::now();
        for (key, expected_name) in [
            (&fx.named_key, &fx.named_principal),
            (&fx.wildcard_key, &fx.wildcard_principal),
        ] {
            let from_file = file_snapshot
                .authenticate(key, now)
                .expect("File mode must resolve this key");
            let from_db = db_snapshot
                .authenticate(key, now)
                .expect("the imported database must resolve the same key");
            assert_eq!(
                from_file.name, from_db.name,
                "the same key must resolve to the same principal name"
            );
            assert_eq!(&from_db.name, expected_name);
            assert_eq!(
                from_file.allow_all, from_db.allow_all,
                "wildcard grants must survive as wildcards, not as an expanded list"
            );

            // One model the key is granted, one it is not, and one that does
            // not exist anywhere — the last is what actually distinguishes a
            // wildcard grant from a list that happened to cover everything.
            for model in [
                fx.granted_model.as_str(),
                fx.ungranted_model.as_str(),
                "a-model-no-config-has-ever-mentioned",
            ] {
                assert_eq!(
                    from_file.may_invoke(model),
                    from_db.may_invoke(model),
                    "authorisation for principal {:?} on model {model:?} disagrees between \
                     File mode and the imported database",
                    from_db.name
                );
            }
        }

        // Guard against the comparison passing vacuously — if both sides
        // said "yes" (or both "no") to everything, the loop above would be
        // satisfied without proving anything about grants.
        let named = db_snapshot.authenticate(&fx.named_key, now).unwrap();
        assert!(named.may_invoke(&fx.granted_model));
        assert!(!named.may_invoke(&fx.ungranted_model));
        let wildcard = db_snapshot.authenticate(&fx.wildcard_key, now).unwrap();
        assert!(wildcard.may_invoke(&fx.ungranted_model));
    }

    /// Re-running import over the same file must converge, not accumulate:
    /// the operator-facing promise is that `import` is safe to run on every
    /// deploy, and a second principal, key or grant per run would quietly
    /// break that.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn re_importing_auth_creates_nothing_new() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let fx = auth_fixture();
        let _cleanup = fx.cleanup();
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&fx.yaml).unwrap();

        import(&pool, &cfg, &test_key()).await.unwrap();
        let second = import(&pool, &cfg, &test_key()).await.unwrap();

        assert_eq!(second.principals, 0);
        assert_eq!(second.principals_existing, 2);
        assert_eq!(second.keys, 0);
        assert_eq!(second.keys_existing, 2);
        assert_eq!(second.grants, 0);
        assert_eq!(second.grants_existing, 2);
        assert_eq!(second.grants_revoked, 0);

        for name in [&fx.named_principal, &fx.wildcard_principal] {
            let principals: i64 =
                sqlx::query_scalar("SELECT count(*) FROM principals WHERE name = $1")
                    .bind(name)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(principals, 1, "principal {name} was duplicated");
            let keys: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM api_keys k JOIN principals p ON p.id = k.principal_id
                 WHERE p.name = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(keys, 1, "key for {name} was duplicated");
        }
    }

    /// Narrowing a key's `models:` and re-importing must actually revoke,
    /// not just stop adding. An import that only ever grants would leave a
    /// deliberately-removed model reachable until someone noticed by hand.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn narrowing_a_grant_list_revokes_the_dropped_model() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let a = unique_name("narrowing-test-keep");
        let b = unique_name("narrowing-test-drop");
        let principal = unique_name("narrowing-principal");
        let key = unique_name("sk-narrowing-key-long-enough-to-prefix");
        let _cleanup = TestCleanup::new()
            .track_exact("models", "name", a.clone())
            .track_exact("models", "name", b.clone())
            .track_exact("principals", "name", principal.clone())
            .track_exact("roles", "name", import_role_name(&principal));

        let models = format!(
            "model_list:\n\
             \x20 - model_name: {a}\n    litellm_params: {{ api_base: http://{a}:8000/v1 }}\n\
             \x20 - model_name: {b}\n    litellm_params: {{ api_base: http://{b}:8000/v1 }}\n"
        );
        let wide: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "{models}auth:\n  keys:\n    - key: {key}\n      name: {principal}\n      models: [{a}, {b}]\n"
        ))
        .unwrap();
        let narrow: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "{models}auth:\n  keys:\n    - key: {key}\n      name: {principal}\n      models: [{a}]\n"
        ))
        .unwrap();

        assert_eq!(import(&pool, &wide, &test_key()).await.unwrap().grants, 2);
        let narrowed = import(&pool, &narrow, &test_key()).await.unwrap();
        assert_eq!(narrowed.grants_revoked, 1);

        let snapshot = crate::control::build::build_snapshot(&pool, &test_key())
            .await
            .unwrap();
        let p = snapshot
            .authenticate(&key, std::time::SystemTime::now())
            .unwrap();
        assert!(p.may_invoke(&a));
        assert!(
            !p.may_invoke(&b),
            "the dropped model must no longer resolve"
        );
    }

    /// The prefix column is returned by `GET /admin/keys`, and an imported
    /// key is whatever the operator wrote — often far shorter than the 67
    /// characters `generate_key` mints. Storing 11 characters of a 7-character
    /// key would put the whole secret in a listing.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_short_imported_key_is_not_stored_in_the_prefix_column() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let principal = unique_name("short-key-principal");
        let _cleanup = TestCleanup::new()
            .track_exact("principals", "name", principal.clone())
            .track_exact("roles", "name", import_role_name(&principal));
        // Short *and* unique: 11 characters, well under the threshold at
        // which any prefix is safe to store, but distinct per run because
        // `api_keys.hash` is UNIQUE across the shared scratch database.
        let key = format!(
            "sk{:09}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000_000
        );
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "auth:\n  keys:\n    - key: {key}\n      name: {principal}\n      models: ['*']\n"
        ))
        .unwrap();
        import(&pool, &cfg, &test_key()).await.unwrap();

        let prefix: String = sqlx::query_scalar(
            "SELECT prefix FROM api_keys k JOIN principals p ON p.id = k.principal_id
             WHERE p.name = $1",
        )
        .bind(&principal)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(prefix, "(hidden)");
        assert!(
            !key.starts_with(&prefix),
            "prefix {prefix:?} discloses the leading characters of a short key"
        );
    }

    /// Regression for the review finding: `import_key` used to resolve a
    /// principal purely by name and reuse whatever it found, so a config
    /// file naming an existing, privileged, hand-created principal minted a
    /// key that silently inherited every role already granted to it — the
    /// grant handling itself (the per-principal `import:<name>` role) was
    /// never the problem; reusing someone else's principal was. This proves
    /// the fix: importing a config that names a pre-existing principal with
    /// an `admin`-equivalent grant must be refused, and that principal's
    /// roles must not reach any key `import` creates.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn import_refuses_to_attach_a_key_to_a_preexisting_principal() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();

        // A principal created by hand (through the admin API's own code
        // path, not by `import`) and granted `admin` — the privileged role
        // whose permissions must never leak to an attacker-controlled config
        // file that merely names it.
        let victim_principal = unique_name("hand-created-admin-principal");
        let _cleanup =
            TestCleanup::new().track_exact("principals", "name", victim_principal.clone());
        let principal_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(&victim_principal)
        .fetch_one(&pool)
        .await
        .unwrap();
        let admin_role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE name = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO principal_roles (principal_id, role_id) VALUES ($1, $2)")
            .bind(principal_id)
            .bind(admin_role_id)
            .execute(&pool)
            .await
            .unwrap();

        let hijack_key = unique_name("sk-hijack-attempt-long-enough-to-prefix");
        let cfg: crate::config::FileConfig = serde_yaml::from_str(&format!(
            "auth:\n  keys:\n    - key: {hijack_key}\n      name: {victim_principal}\n      models: ['*']\n"
        ))
        .unwrap();

        let err = import(&pool, &cfg, &test_key())
            .await
            .expect_err("import must refuse to attach a new key to a principal it did not create");
        assert!(
            err.to_string().contains(&victim_principal),
            "the error must name the principal that could not be reused: {err}"
        );

        // Nothing must have been created for the attempted key at all — not
        // the key, and no grant either.
        let key_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM api_keys WHERE hash = $1)")
                .bind(hash_key(&hijack_key).to_vec())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !key_exists,
            "the rejected import must not have created a key"
        );

        // The victim principal's own roles must be exactly what they were:
        // still just `admin`, not also `import:<name>`.
        let roles: Vec<String> = sqlx::query_scalar(
            "SELECT r.name FROM principal_roles pr JOIN roles r ON r.id = pr.role_id
             WHERE pr.principal_id = $1",
        )
        .bind(principal_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(roles, vec!["admin".to_string()]);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn reencrypt_migrates_a_plaintext_row_and_is_idempotent() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let key = test_key();
        let _cleanup = TestCleanup::new().track_prefix("models", "name", "legacy-plaintext-");

        let name = unique_name("legacy-plaintext");

        // Inserting the plaintext row and migrating it happen inside one
        // transaction, committed only once both are done. `build_snapshot`
        // (used by other tests sharing this same scratch database, and by
        // the periodic rebuild in `control::api`) decrypts every non-null
        // `upstream_api_key` row it sees table-wide; committing the raw
        // plaintext row on its own, even briefly, would be a real window in
        // which a concurrent `build_snapshot` call legitimately fails on
        // this test's fixture data. Read-committed isolation means no other
        // connection ever sees a row that was inserted and migrated inside
        // the same uncommitted transaction.
        let mut tx = pool.begin().await.unwrap();
        // Simulate a row written by the pre-encryption code path: raw
        // plaintext bytes, exactly as the old `import` stored them. The
        // credential lives on the provider since migration 0029, so that is
        // where the legacy shape has to be reproduced.
        let provider_id: i64 = sqlx::query_scalar(
            "INSERT INTO providers (name, api_base, upstream_api_key) \
             VALUES ($1, 'http://legacy:8000/v1', $2) RETURNING id",
        )
        .bind(&name)
        .bind(b"sk-legacy-plaintext-token".to_vec())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let model_id: i64 = sqlx::query_scalar(
            "INSERT INTO models (name, provider_id, upstream_model) \
             VALUES ($1, $2, 'legacy-model') RETURNING id",
        )
        .bind(&name)
        .bind(provider_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();

        // Scoped to this test's own model: the shared dev database also holds
        // real providers encrypted under the deployment's key, which this
        // test's throwaway key cannot decrypt — and the unscoped sweep is
        // right to refuse those rather than guess.
        let migrated = reencrypt_plaintext_backends_scoped(&mut tx, &key, Some(model_id))
            .await
            .unwrap();
        assert!(migrated >= 1);
        tx.commit().await.unwrap();

        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT upstream_api_key FROM providers WHERE id = $1")
                .bind(provider_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(stored, b"sk-legacy-plaintext-token".to_vec());
        assert_eq!(
            secrets::decrypt(&key, &stored).unwrap(),
            "sk-legacy-plaintext-token"
        );

        // Running it again must be a no-op: the row is already ciphertext.
        let migrated_again = reencrypt_plaintext_backends_scoped(&pool, &key, Some(model_id))
            .await
            .unwrap();
        let stored_again: Vec<u8> =
            sqlx::query_scalar("SELECT upstream_api_key FROM providers WHERE id = $1")
                .bind(provider_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored, stored_again,
            "already-migrated row must be left alone"
        );
        let _ = migrated_again;
    }
}
