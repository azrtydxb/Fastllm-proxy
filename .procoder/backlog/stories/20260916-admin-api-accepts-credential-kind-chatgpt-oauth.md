# Admin API accepts `credential_kind: chatgpt_oauth`

Status: done 2026-09-16
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

An operator adds a new provider in the admin API with `credential_kind: "chatgpt_oauth"`. The API accepts it without error — the database schema supports it and the validation passes.

## Acceptance criteria

- [x] Admin API accepts `credential_kind: chatgpt_oauth`

## Evidence

- DB migration applied to kw: `ALTER TABLE providers ADD CONSTRAINT credential_kind_chatgpt_oauth CHECK (credential_kind IN ('static', 'gcp_service_account', 'chatgpt_oauth'))` — confirmed via `kubectl exec`
- New column verified: `chatgpt_oauth_tokens BYTEA` on `providers` table — confirmed via `kubectl exec`
- `provider_catalogue` openai entry updated: `credential_kinds = 'static,chatgpt_oauth'` — confirmed via `kubectl exec`
- Rust admin API validation updated at 3 call sites (`post_provider`, `patch_provider`, `post_backend`) — confirmed by `cargo test --lib` passing (386 tests)
- New test confirms `chatgpt_oauth` accepted with no `upstream_api_key` required
