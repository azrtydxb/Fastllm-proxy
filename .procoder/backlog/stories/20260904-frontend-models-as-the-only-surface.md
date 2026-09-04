# Frontend models become the only client-facing surface

Status: open
Created: 2026-09-04
Epic: rbac-on-frontend-models
Sprint: sprint-6

## Description

As an operator, a caller can only name something I have deliberately exposed.

If a client can still name a provider model directly, frontend-only grants leave a bypass — the caller names the concrete model and misses the layer that governs it. Per-provider names already make direct addressing ambiguous, so this is the same decision as the one in the decomposition epic and has to land with it.

A `model/*` wildcard must also stop meaning "anything a provider happens to expose", which is what makes a new dynamic model silently reachable.

There is a countable migration step underneath this, not just a policy change. On the dev cluster three provider models have no frontend model in front of them — `gpt-5`, `gemini-2.5-flash` and `openrouter-free` — and callers name them directly today; the verification request for the provider split named `gpt-5` itself. Making frontend models the only addressable surface without giving those a frontend model first is a silent 404 for every one of their callers. The migration has to create one per concrete model that lacks it, named after the model, the same shape 0029 already produces for a split.

## Acceptance criteria

- [ ] A request naming a provider model is refused, or resolves only through a qualified form the ADR specifies
- [ ] `model:invoke` on a wildcard cannot reach a provider model that no frontend model exposes
- [ ] Every provider model that callers can name today has a frontend model in
      front of it before addressing changes — three on the dev cluster do not
- [ ] `GET /v1/models` lists frontend models only
- [ ] Verified on kw with a scoped principal: the frontend model succeeds, its underlying provider model is unreachable by name

## Evidence
