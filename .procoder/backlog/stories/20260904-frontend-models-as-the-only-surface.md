# Frontend models become the only client-facing surface

Status: open
Created: 2026-09-04
Epic: rbac-on-frontend-models
Sprint: sprint-6

## Description

As an operator, a caller can only name something I have deliberately exposed.

If a client can still name a provider model directly, frontend-only grants leave a bypass — the caller names the concrete model and misses the layer that governs it. Per-provider names already make direct addressing ambiguous, so this is the same decision as the one in the decomposition epic and has to land with it.

A `model/*` wildcard must also stop meaning "anything a provider happens to expose", which is what makes a new dynamic model silently reachable.

## Acceptance criteria

- [ ] A request naming a provider model is refused, or resolves only through a qualified form the ADR specifies
- [ ] `model:invoke` on a wildcard cannot reach a provider model that no frontend model exposes
- [ ] `GET /v1/models` lists frontend models only
- [ ] Verified on kw with a scoped principal: the frontend model succeeds, its underlying provider model is unreachable by name

## Evidence
