-- 0001_init.sql documented `model_backends.upstream_api_key` as "Encrypted
-- at rest and never returned by the admin API" — but no encryption has ever
-- been implemented anywhere in this codebase; the column has always stored
-- the credential as-is. 0001 is already applied in every live deployment
-- (sqlx checksums migration files, so it cannot be edited in place without
-- breaking every environment that already ran it), so the correction lands
-- here instead, as a schema comment a reader will actually see via \d+.
--
-- This is intentionally NOT an attempt to add encryption: key management and
-- rotation are a feature in their own right, out of scope here, and a
-- hand-rolled scheme now would be worse than being honest about having none.
COMMENT ON COLUMN model_backends.upstream_api_key IS
    'Upstream bearer token for this backend, stored AS-IS — NOT encrypted at '
    'rest, despite what an earlier version of this comment claimed. Never '
    'returned by the admin API, but carried in the /snapshot wire format '
    'because the proxy must present it to the backend, so /snapshot must be '
    'TLS wherever a backend has a real credential. Read access to this '
    'column is read access to upstream credentials in plaintext; there is '
    'no encryption-at-rest layer in this codebase today.';
