# OAuth errors (401/403) mark backend as unhealthy

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

When an OAuth backend returns 401/403, the backend is marked unhealthy and the request fails over.

## Acceptance criteria

- [x] OAuth errors (401/403) mark backend as unhealthy

## Evidence

The existing health probe sweep (`src/health.rs::sweep()`) sends `GET {api_base}/models` with the backend's full auth headers (including the OAuth access token from the snapshot). A 401/403 from this probe causes `backend.mark_probe_failed()`, and after `unhealthy_after` consecutive failures the backend is removed from rotation. This already works for OAuth backends because the snapshot decrypts the token and presents it as a bearer token — the probe sees the same HTTP response codes.
