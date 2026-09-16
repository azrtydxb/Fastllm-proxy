# Disconnect clears stored tokens

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

The admin UI can disconnect an OAuth provider, clearing its stored tokens.

## Acceptance criteria

- [x] Disconnect clears stored tokens

## Evidence

`POST /admin/providers/{id}/oauth/disconnect` calls `oauth::disconnect()` which sets `chatgpt_oauth_tokens = NULL`. The API endpoint is registered in `src/control/api.rs` and the implementation is in `src/control/oauth.rs`.
