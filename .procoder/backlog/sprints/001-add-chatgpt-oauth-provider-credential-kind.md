# Add ChatGPT OAuth provider credential kind

Status: closed 2026-09-16
Created: 2026-09-16

## Goal

A user can add a ChatGPT Pro subscription as a provider in FastLLM, connect
it via OAuth, and have its tokens auto-refreshed — routing through it and
falling over on OAuth errors the same as any other backend.

## Retrospective

### What slowed us down

- The provider_catalogue entry was already done as part of Story 1's migration,
  so Story 2 (catalogue entry) was a no-op — we can collapse those two.
- OpenAI's OAuth endpoints are not public documentation; we had to infer them
  from OpenClaw's implementation and ChatGPT's developer docs. This makes
  testing the OAuth flow against a real endpoint impossible until we verify
  the auth URLs.

### What we change next sprint

- Combine catalogue + admin API into one story where possible — they share
  the migration.
- Research OpenAI's OAuth endpoints before coding: auth URL, token URL,
  scopes, challenge response format. Document in an ADR.

### One adaptation worth keeping

- Applying migrations directly to kw via `kubectl exec` worked well when
  `sqlx migrate` couldn't reach the cluster (no TLS, no port-forward). Keep
  this pattern for future DB changes.

## Result

committed: 1
done: 1 (20260916-admin-api-accepts-credential-kind-chatgpt-oauth)
carried: 0
