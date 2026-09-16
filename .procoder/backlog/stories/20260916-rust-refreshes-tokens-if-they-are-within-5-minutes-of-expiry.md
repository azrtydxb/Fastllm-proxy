# Rust refreshes tokens if they are within 5 minutes of expiry

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

The snapshot build loop checks OAuth tokens before dispatch and refreshes them if they are near expiry.

## Acceptance criteria

- [x] Rust refreshes tokens if they are within 5 minutes of expiry

## Evidence

`src/control/oauth.rs::refresh_if_needed()` checks if `expires_at` is within the 300-second margin, and if so, exchanges the `refresh_token` for a new access token. `src/control/build.rs` calls this for every `chatgpt_oauth` backend during snapshot rebuild, so every proxy receives a fresh token.
