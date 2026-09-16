# chatgpt-oauth-provider — implementation plan

Status: draft
Spec: .procoder/specs/chatgpt-oauth-provider.md

## Goal

Add a `chatgpt_oauth` credential kind so a provider model can authenticate
against OpenAI's ChatGPT subscription backend without an API key.

## Architecture

- New DB column `providers.chatgpt_oauth_tokens` (encrypted BYTEA, like
  `upstream_api_key`) and new CHECK value on `credential_kind`
- New `provider_catalogue` row for OpenAI ChatGPT with `credential_kinds`
  including `chatgpt_oauth`
- Control plane: connect/disconnect/status API endpoints; OAuth challenge/
  callback/token-exchange flow
- Rust snapshot: decrypt tokens, refresh if needed, build auth headers like
  the existing `upstream_api_key` path
- Background refresh task in the snapshot-rebuild loop

## Constraints

- Never store or transmit the user's ChatGPT password
- Tokens encrypted at rest using the existing AES-256-GCM mechanism
- Rust hot path must not perform I/O for auth — tokens decrypted before
  request dispatch in the snapshot
- Streaming must remain end-to-end, not buffered

---

## Task 1: Database migration — new column and CHECK value

Files:
- `migrations/0051_chatgpt_oauth_provider.sql`

Interfaces:
- `providers.credential_kind` CHECK constraint includes `chatgpt_oauth`
- `providers.chatgpt_oauth_tokens` BYTEA — encrypted OAuth token blob

Steps:
- [ ] ALTER TABLE providers ADD COLUMN chatgpt_oauth_tokens BYTEA DEFAULT NULL
- [ ] ALTER TABLE providers ADD CONSTRAINT credential_kind_chatgpt_oauth
    CHECK (credential_kind IN ('static', 'gcp_service_account', 'chatgpt_oauth'))
- [ ] COMMENT ON COLUMN providers.chatgpt_oauth_tokens (describe encrypted
  JSON: access_token, refresh_token, token_type, expires_in, created_at)
- [ ] Run `DATABASE_URL=$(cat /tmp/dburl) sqlx migrate run` on kw to verify
  it applies cleanly (or test with local postgres if available)

## Task 2: Provider catalogue entry for OpenAI ChatGPT

Files:
- `migrations/0051_chatgpt_oauth_provider.sql` (appends to the INSERT)

Interfaces:
- `provider_catalogue` gains an `openai-chatgpt` entry
- `credential_kinds` includes `chatgpt_oauth` as the first value

Steps:
- [ ] INSERT INTO provider_catalogue VALUES (... openai-chatgpt ..., 'openai', 'authorization', 'Bearer', NULL) ON CONFLICT DO NOTHING
- [ ] `credential_kinds = 'chatgpt_oauth,static'` for the new entry
- [ ] Verify `tests/doc_claims.rs` still passes (the count increases by one)

## Task 3: Rust — struct and enum updates for the new credential kind

Files:
- `src/snapshot.rs` — `BackendDef` gains `chatgpt_oauth_tokens: Option<String>`
- `src/registry.rs` — `BackendDef` parsing from snapshot
- `src/config.rs` — credential_kind validation

Interfaces:
- `BackendDef` carries `chatgpt_oauth_tokens` (decrypted)
- When `chatgpt_oauth_tokens` is present, `Backend::new()` uses it to build
  auth headers instead of `api_key`

Steps:
- [ ] Add `chatgpt_oauth_tokens: Option<String>` to `BackendDef` in `snapshot.rs`
- [ ] In `BackendDef` Default impl, add `chatgpt_oauth_tokens: None`
- [ ] In `Backend::new()`, when `chatgpt_oauth_tokens.is_some()` build
  headers from the decrypted access token with `Authorization: Bearer <token>`
- [ ] Add `credential_kind: CredentialKind` enum with `Static`,
  `GcpServiceAccount`, `ChatGptOauth` variants
- [ ] Update the control-plane-to-snapshot path to carry the new field

## Task 4: Control plane — OAuth connect/status/disconnect APIs

Files:
- `src/control/api.rs` — new endpoints
- `src/control/oauth.rs` — new module (PKCE, token exchange, storage)

Interfaces:
- `POST /admin/providers/:id/chatgpt-connect` → returns `{challenge_url, state}`
- `GET /admin/providers/:id/oauth/callback?code=...&state=...` → stores tokens
- `GET /admin/providers/:id/chatgpt-status` → returns `{connected: bool, last_refresh: ...}`
- `POST /admin/providers/:id/chatgpt-disconnect` → clears tokens

Steps:
- [ ] Create `src/control/oauth.rs` with PKCE challenge generation
  (random 43+ char verifier → SHA256 → base64url code challenge)
- [ ] `POST /admin/providers/:id/chatgpt-connect` — generate state, store in
  session/cache, redirect URL to OpenAI auth endpoint with PKCE params
- [ ] Callback handler — validate state, exchange code + verifier for tokens
  from `https://auth.beehive.chatgpt.com/v1/oauth/token`, encrypt and store
  in `providers.chatgpt_oauth_tokens`
- [ ] `POST /admin/providers/:id/chatgpt-disconnect` — SET NULL on
  `chatgpt_oauth_tokens`
- [ ] `GET /admin/providers/:id/chatgpt-status` — read `chatgpt_oauth_tokens`
  to report connection state

## Task 5: Rust — token refresh in the snapshot build

Files:
- `src/registry.rs` — refresh logic in `BackendDef` build
- `src/control/build.rs` — snapshot build calls refresh if needed

Interfaces:
- If `expires_in` is within 5 minutes of expiry, call refresh endpoint
  (in the snapshot-build path, before the snapshot is sent to data plane)
- The data plane only sees a valid access token

Steps:
- [ ] In `registry.rs::build()` or the snapshot build path, after decrypting
  `chatgpt_oauth_tokens`, parse the JSON, check `expires_in`
- [ ] If `now + 5min >= expires_at`, exchange refresh token for new access
  token via `POST /admin/providers/:id/chatgpt-refresh`
- [ ] Store new access token back in DB (update `chatgpt_oauth_tokens`)
- [ ] If refresh fails, keep old token (the data plane will see 401 and mark
  unhealthy, triggering failover)
- [ ] If both access and refresh tokens are expired, mark the provider as
  needing reconnection (status.connected = false)

## Task 6: Health check and error handling for OAuth

Files:
- `src/health.rs` — no changes (health checks are the same for all backends)
- `src/proxy.rs` — 401/403 from an OAuth backend triggers unhealthy

Interfaces:
- Same as existing: 401/403/429 from an OAuth backend → mark unhealthy →
  failover
- No special routing changes needed

Steps:
- [ ] Verify that existing `backend.note_error()` on non-2xx responses
  correctly marks OAuth backends as unhealthy
- [ ] Add test: an OAuth backend returning 401 is excluded from routing
- [ ] Add test: an OAuth backend returning 429 triggers failover

## Task 7: Tests

Files:
- `tests/chatgpt_oauth.rs` — new integration test
- `src/registry.rs` tests — token parsing and refresh timing

Steps:
- [ ] Unit test: token JSON parsing (access_token, refresh_token, expires_in,
  created_at)
- [ ] Unit test: refresh triggered when < 5 min from expiry
- [ ] Unit test: no refresh when > 5 min from expiry
- [ ] E2E test (ignored): mock OAuth provider, connect a provider, send a
  request, verify it succeeds through the OAuth flow

## Task 8: Docs

Files:
- `docs/providers.md` — add ChatGPT/OAuth section
- `docs/architecture.md` — update the auth flow section

Steps:
- [ ] Add a new section to `docs/providers.md` for the ChatGPT OAuth path
- [ ] Update `docs/architecture.md` to mention `chatgpt_oauth` in the
  provider/credential list
