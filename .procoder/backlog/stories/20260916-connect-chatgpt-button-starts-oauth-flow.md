# Connect ChatGPT button starts OAuth flow

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

The admin UI can trigger the OAuth connect flow for a provider, producing a PKCE challenge URL.

## Acceptance criteria

- [x] Connect ChatGPT button starts OAuth flow

## Evidence

`POST /admin/providers/{id}/oauth/connect` calls `oauth::generate_challenge()` which returns a PKCE challenge URL (`challenge_url` field). The operator opens this in a browser, authorizes, and is redirected to the callback. Implemented in `src/control/api.rs` alongside the other OAuth endpoints.
