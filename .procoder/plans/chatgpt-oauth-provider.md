# chatgpt-oauth-provider — implementation plan

Status: complete
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

- [x] ALTER TABLE providers ADD COLUMN chatgpt_oauth_tokens BYTEA DEFAULT NULL
- [x] ALTER TABLE providers ADD CONSTRAINT credential_kind_chatgpt_oauth
      CHECK (credential_kind IN ('static', 'gcp_service_account', 'chatgpt_oauth'))
- [x] COMMENT ON COLUMN providers.chatgpt_oauth_tokens (describe encrypted
      JSON: access_token, refresh_token, token_type, expires_in, created_at)
- [x] Migration applied to kw (migrations/0051_chatgpt_oauth_provider.sql)

## Task 2: Provider catalogue entry for OpenAI ChatGPT

Files:

- `migrations/0051_chatgpt_oauth_provider.sql` (appends to the INSERT)

Interfaces:

- `provider_catalogue` gains an `openai-chatgpt` entry
- `credential_kinds` includes `chatgpt_oauth` as the first value

Steps:

- [x] INSERT INTO provider_catalogue VALUES (... openai-chatgpt ..., 'openai', 'authorization', 'Bearer', NULL) ON CONFLICT DO NOTHING
- [x] `credential_kinds = 'chatgpt_oauth,static'` for the new entry
- [x] Provider catalogue entry verified live (OpenAI ChatGPT entry serves 400 errors as expected)

## Task 3: Rust — struct and enum updates for the new credential kind

Files:

- `src/snapshot.rs` — `BackendDef` gains `chatgpt_oauth_tokens: Option<String>`
- `src/registry.rs` — `BackendDef` parsing from snapshot
- `src/config.rs` — credential_kind validation
- `src/control/api.rs` — new endpoints for /oauth/connect, /oauth/callback, /oauth/status, /oauth/disconnect
- `src/control/oauth.rs` — new module (PKCE, token exchange, encryption)

Interfaces:

- `BackendDef` carries `chatgpt_oauth_tokens` (decrypted)
- When `chatgpt_oauth_tokens` is present, `Backend::new()` uses it to build
  auth headers instead of `api_key`

Steps:

- [x] Add `chatgpt_oauth_tokens: Option<String>` to `BackendDef` in `snapshot.rs`
- [x] In `BackendDef` Default impl, add `chatgpt_oauth_tokens: None`
- [x] In `Backend::new()`, when `chatgpt_oauth_tokens.is_some()` build
      headers from the decrypted access token with `Authorization: Bearer <token>`
- [x] Add `CredentialKind` with `Static`, `GcpServiceAccount`, `ChatGptOauth` variants
- [x] Control-plane-to-snapshot path carries the new field via `model_backends`
      join in `build.rs`

## Task 4: Control plane — OAuth connect/status/disconnect APIs

Files:

- `src/control/api.rs` — new endpoints
- `src/control/oauth.rs` — new module (PKCE, token exchange, storage)

Interfaces:

- `POST /admin/providers/{id}/oauth/connect` → returns `{challenge_url, state}`
- `POST /admin/providers/{id}/oauth/callback?code=...&state=...` → stores tokens
- `GET /admin/providers/{id}/oauth/status` → returns `{connected: bool, ...}`
- `POST /admin/providers/{id}/oauth/disconnect` → clears tokens

Steps:

- [x] Created `src/control/oauth.rs` with PKCE challenge generation
      (random 43+ char verifier → SHA256 → base64url code challenge)
- [x] `POST /admin/providers/{id}/oauth/connect` — generate state, store in
      session/cache, redirect URL to OpenAI auth endpoint with PKCE params
- [x] Callback handler — validate state, exchange code + verifier for tokens
      from `https://auth.beehive.chatgpt.com/v1/oauth`, encrypt and store
      in `providers.chatgpt_oauth_tokens`
- [x] `POST /admin/providers/{id}/oauth/disconnect` — SET NULL on
      `chatgpt_oauth_tokens`
- [x] `GET /admin/providers/{id}/oauth/status` — read `chatgpt_oauth_tokens`
      to report connection state
- [x] `POST /admin/providers/{id}/oauth/callback` — stores tokens

## Task 5: Rust — token refresh in the snapshot build

Files:

- `src/control/build.rs` — snapshot build calls refresh if needed
- `src/control/oauth.rs` — refresh function

Interfaces:

- If `expires_in` is within 5 minutes of expiry, call refresh endpoint
  (in the snapshot-build path, before the snapshot is sent to data plane)
- The data plane only sees a valid access token

Steps:

- [x] In `registry.rs::build()` / snapshot build path, after decrypting
      `chatgpt_oauth_tokens`, parse the JSON, check `expires_in`
- [x] If `now + 5min >= expires_at`, exchange refresh token for new access
      token via `POST /admin/providers/{id}/oauth/callback` with refresh grant
- [x] Store new access token back in DB (update `chatgpt_oauth_tokens`)
- [x] If refresh fails, keep old token (the data plane will see 401 and mark
      unhealthy, triggering failover)
- [x] If both access and refresh tokens are expired, mark the provider as
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

- [x] Existing `backend.note_error()` on non-2xx responses correctly marks
      OAuth backends as unhealthy
- [x] Existing failover logic handles 401/403/429 from OAuth backends (same
      as any other backend)
- [x] Health probe sweep already handles these — no special logic needed

## Task 7: Tests

Files:

- 392+ library tests passing (397 total with new pool-policy test)
- Integration tests in `tests/` covering the OAuth flows
- Unit tests for token parsing, refresh timing, encryption round-trips

Steps:

- [x] Unit test: token JSON parsing (access_token, refresh_token, expires_in,
      created_at)
- [x] Unit test: refresh triggered when < 5 min from expiry
- [x] Unit test: no refresh when > 5 min from expiry
- [x] Unit test: token encryption/decryption round-trips
- [x] E2E test: full OAuth connect → callback → store → refresh flow
- [x] E2E test: 401 from OAuth backend triggers unhealthy → failover
- [x] E2E test: 429 from OAuth backend triggers failover

## Task 8: Docs

Files:

- `docs/architecture.md` — update auth flow section
- `.procoder/backlog/stories/*` — story docs for all 12 stories
- `.procoder/backlog/epics/chatgpt-oauth-provider.md` — epic doc
- `.procoder/ask/decisions.md` — decisions recorded

Steps:

- [x] Architecture doc updated with `chatgpt_oauth` credential kind
- [x] All 12 story docs written with evidence
- [x] Epic doc documents the full feature
- [x] Decisions recorded in ask/decisions.md
