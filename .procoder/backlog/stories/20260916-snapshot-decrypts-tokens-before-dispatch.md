# Snapshot decrypts tokens before dispatch

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

OAuth tokens are decrypted in the snapshot build and the plain access token travels in the snapshot to proxies.

## Acceptance criteria

- [x] Snapshot decrypts tokens before dispatch

## Evidence

`src/control/build.rs` calls `oauth::refresh_if_needed(pool, key, backend_id)` for `chatgpt_oauth` backends. This decrypts the tokens, refreshes if needed, and returns a plain access token that becomes the `BackendDef::api_key` in the snapshot. The same pattern GCP service accounts use.
