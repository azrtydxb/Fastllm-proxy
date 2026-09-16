# PKCE challenge/verifier generated server-side

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

The OAuth challenge (SHA256 of a random verifier) is generated server-side, never sent to the client.

## Acceptance criteria

- [x] PKCE challenge/verifier generated server-side

## Evidence

`src/control/oauth.rs::generate_challenge()` generates a 48-character random `code_verifier` and computes `code_challenge = BASE64URL(SHA256(verifier))`. The verifier is stored in-memory (not sent to the client) and used only for the token exchange. Tests in `src/control/oauth.rs` verify the challenge is deterministic and follows RFC 7636.
