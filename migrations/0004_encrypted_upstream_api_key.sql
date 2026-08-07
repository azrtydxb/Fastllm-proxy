-- migrations/0002 corrected the schema comment on this column to admit there
-- was no encryption-at-rest layer at all. That is no longer true:
-- `control::secrets` (AES-256-GCM via `ring::aead`) now encrypts
-- `upstream_api_key` before `control::import::import` ever writes it, and
-- `control::build::build_snapshot` decrypts it back on read. This migration
-- only updates the comment — the column stays BYTEA, no data is touched by
-- running it — but see the note below about rows that already exist.
--
-- ## Existing plaintext rows
--
-- A row written by `import` before this change holds the raw credential
-- bytes, not an `encrypt`-produced blob, and neither `import` (idempotent on
-- (model_id, api_base, upstream_model), not on the key's content — it will
-- not touch or re-encrypt an existing row) nor `build_snapshot` will
-- silently reinterpret old rows for you; `build_snapshot` now requires every
-- non-null `upstream_api_key` to be a valid ciphertext blob and returns an
-- error if it isn't.
--
-- Migrating them is a one-shot, explicit operator action, not a lazy
-- upgrade-on-next-read baked into the read path:
--
--   fastllm-proxy reencrypt-backends --database-url "$FASTLLM_DATABASE_URL"
--
-- (requires FASTLLM_ENCRYPTION_KEY in the environment, same as `import` and
-- `--role control`/`all`). It is safe to run more than once and safe to run
-- against a database with no plaintext rows — see
-- `control::import::reencrypt_plaintext_backends` for how it tells an
-- already-migrated row apart from a pre-migration one.
--
-- This was not made a data-migrating SQL statement in this file because a
-- SQL migration has no access to FASTLLM_ENCRYPTION_KEY or an AES-GCM
-- implementation — encryption has to happen in the application, not in a
-- migration runner. As of this migration landing, the live cluster has no
-- control-plane database deployed yet at all (see deploy/README.md), so the
-- realistic case for `reencrypt-backends` is a developer's scratch database
-- that predates this change, not a production cutover.
COMMENT ON COLUMN model_backends.upstream_api_key IS
    'Upstream bearer token for this backend, encrypted at rest with '
    'AES-256-GCM (control::secrets; version_byte || nonce || ciphertext+tag). '
    'This protects the database, not the /snapshot wire format: the proxy '
    'still receives it decrypted and usable, because it must present it to '
    'the backend as a bearer token, so /snapshot must still be TLS wherever '
    'a backend has a real credential. Never returned by the admin API. Rows '
    'written before this migration may still hold pre-encryption plaintext '
    '-- see this migration file for the one-shot `reencrypt-backends` '
    'command that migrates them.';
