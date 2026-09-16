# Callback exchange produces access and refresh tokens

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

After the user authorizes on OpenAI, the callback handler exchanges the code for tokens.

## Acceptance criteria

- [x] Callback exchange produces access and refresh tokens

## Evidence

`src/control/oauth.rs::exchange_token()` posts the authorization code + verifier to OpenAI's token endpoint (`https://auth.beehive.chatgpt.com/v1/oauth/token`) and extracts `access_token` and `refresh_token` from the response. These are serialized to JSON, encrypted, and stored in the `providers.chatgpt_oauth_tokens` column. The admin API endpoint `POST /admin/providers/{id}/oauth/callback` wires this up.
