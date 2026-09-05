# Frontend models become the only client-facing surface

Status: done 2026-09-05
Created: 2026-09-04
Epic: rbac-on-frontend-models
Sprint: sprint-6

## Description

As an operator, a caller can only name something I have deliberately exposed.

If a client can still name a provider model directly, frontend-only grants leave a bypass — the caller names the concrete model and misses the layer that governs it. Per-provider names already make direct addressing ambiguous, so this is the same decision as the one in the decomposition epic and has to land with it.

A `model/*` wildcard must also stop meaning "anything a provider happens to expose", which is what makes a new dynamic model silently reachable.

There is a countable migration step underneath this, not just a policy change. On the dev cluster three provider models have no frontend model in front of them — `gpt-5`, `gemini-2.5-flash` and `openrouter-free` — and callers name them directly today; the verification request for the provider split named `gpt-5` itself. Making frontend models the only addressable surface without giving those a frontend model first is a silent 404 for every one of their callers. The migration has to create one per concrete model that lacks it, named after the model, the same shape 0029 already produces for a split.

## Acceptance criteria

- [x] A request naming a provider model is refused, or resolves only through a qualified form the ADR specifies
- [x] `model:invoke` on a wildcard cannot reach a provider model that no frontend model exposes
- [x] Every provider model that callers can name today has a frontend model in
      front of it before addressing changes — three on the dev cluster do not
- [x] `GET /v1/models` lists frontend models only
- [x] Verified on kw with a scoped principal: the frontend model succeeds, its underlying provider model is unreachable by name

## Evidence

- A request naming a provider model is refused: **404** on kw for
  `bge-m3@192.168.10.245:8890`, with a message saying clients name frontend
  models and provider models are inventory.
- `GET /v1/models` lists frontend models only — verified live, no qualified
  names in the response.
- `model:invoke` on a wildcard cannot reach a provider model, because a
  provider model is not a name a request can carry at all.
- Every provider model callers could name already had a frontend model in
  front of it before this landed (migration 0034), so nothing became
  unreachable: the frontend model shadows it and routes straight back.
- `File` mode is carved out. It has no frontend models — routing is a control
  plane feature — so there the model _is_ the client-facing name. Without the
  carve-out every File-mode deployment would have been unable to serve
  anything; found by reading `tests/rbac.rs`, which runs in that mode.
- Pinned in two suites: naming a provider model is 404 in `virtual_models.rs`
  and `failover.rs`, and the fallback does not make one addressable either.
