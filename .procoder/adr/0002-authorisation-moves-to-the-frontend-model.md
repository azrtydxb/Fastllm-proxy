# 0002 — Authorisation moves to the frontend model

Status: accepted
Date: 2026-09-04

## Context

`src/proxy.rs:267-278` authorises against the resolved concrete model, never the
frontend name, and pins it with
`a_virtual_models_grant_does_not_reach_its_targets_and_vice_versa`:

> A virtual model routes access; it must never be able to grant it.

The stated risk is that someone edits a frontend model's rules to point at a
model the caller should not reach, escalating privilege through a routing screen.

Two things force a re-examination. First, that actor cannot exist: editing rules
requires `config:write`, which `src/control/api.rs:4394` documents as covering
"principals, roles, models, backends, virtual models, routing rules and targets,
limits, budgets, passwords" and which is all-or-nothing —
`validate_grant("config:write", "model/x")` is an error and only `*` validates
(`api.rs:7677-7678`). Anyone who can redirect a frontend model can grant
themselves the model directly, create a principal, or set a password.

Second, ADR 0001 plus automatic registration make concrete models ephemeral.
Grants pinned to them evaporate on every model swap and do not return when the
model does, and a `model/*` wildcard silently extends to whatever a provider
starts exposing next — unaudited, because from the permission system's view
nothing was granted; a resource appeared that an existing grant matched.

## Decision

Authorisation is checked against the frontend model the caller named. Provider
models stop being a grantable subject, and frontend models become the only
client-facing surface — a caller cannot name a provider model directly.

Those two halves are one decision. Frontend-only grants with concrete addressing
still available would leave a bypass: name the provider model, miss the layer
that governs it. And concrete addressing was already made ambiguous by ADR
0001's per-provider names.

The alternative of keeping concrete authorisation and giving dynamic models a
stable grantable identity — `provider/spark-246/<name>` — was considered. It
preserves the existing invariant, but it grants access to inventory rather than
to an exposure, and it leaves the wildcard problem intact: a grant on
`provider/spark-246/*` still widens silently when someone starts a new model
there.

## Consequences

Grants survive model swaps, because frontend models are authored by a human and
never auto-removed. New inventory is closed by default: a model a provider starts
exposing reaches nobody until someone points a frontend model at it.

The price is a deliberate reversal of a pinned invariant. The existing test must
be replaced rather than deleted, and the reasoning above has to travel with it,
or a future reader will restore the old behaviour on the strength of the comment
alone.

Every existing grant on a concrete model — `novagrade` holds `model:invoke` on
`model/qwen3-6-35b-a3b-nvfp4` — has to be migrated onto a frontend model, and
any caller naming a concrete model directly breaks.

Provider credentials are unaffected: they are not our RBAC. The provider or
engine defines them; we hold one and present it.
