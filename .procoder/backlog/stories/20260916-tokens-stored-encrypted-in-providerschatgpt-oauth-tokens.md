# Tokens stored encrypted in `providers.chatgpt_oauth_tokens`

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

OAuth tokens (access + refresh) are stored encrypted in the new column, not returned by the admin API.

## Acceptance criteria

- [x] Tokens stored encrypted in `providers.chatgpt_oauth_tokens`

## Evidence

`migrations/0051_chatgpt_oauth_provider.sql` adds the `chatgpt_oauth_tokens BYTEA` column. `src/control/oauth.rs` encrypts tokens via `secrets::encrypt()` before writing to the database (both in `exchange_token` and `refresh_if_needed`).
