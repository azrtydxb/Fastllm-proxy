# 0001 — A provider model belongs to exactly one provider

Status: accepted
Date: 2026-09-04

## Context

`model_backends` gives a model a list of backends, and six of its columns
describe the endpoint rather than the model: `api_base`, `upstream_api_key`,
`protocol`, `auth_header`, `auth_scheme` (`migrations/0001_init.sql:62-72` and
`0013`). A "provider" is not a record at all — `web/src/views/Providers.jsx:23`
says it "is a grouping this screen invents, not a thing the API models" and
derives it by grouping backends by `api_base` origin at render time.

Two forces made this untenable. Automating registration needs a provider to be
something a service can register, heartbeat and expire, which a render-time
grouping cannot be. And the existing shape already has a cost: one OpenRouter
key is encrypted into one row per model, so rotating it is N writes, while the
screen truthfully presents "credential set" as a provider-level fact.

The fork: keep a model owning many backends and bolt a provider concept
alongside it, or make the provider the owner and give each model exactly one.

## Decision

A provider owns the endpoint, the credential, the protocol and the auth scheme.
A provider model belongs to exactly one provider. `model_backends` folds away.
Two providers serving the same model are two provider models.

The alternative — a model with many backends, each pointing at a provider —
was rejected because it keeps two places where "which endpoint serves this"
is answered, and leaves balancing split between the model level (`policy`) and
the frontend level (weighted targets). One of those has to win, and the frontend
already has the richer mechanism: conditions, ordering and weights.

Name uniqueness follows rather than being chosen: two Sparks both expose
`bge-m3`, so names are unique per provider and frontend models become the only
global namespace.

## Consequences

Rotating a provider credential becomes one write. A provider can exist with zero
models, which is what lets a host register before a model has finished loading.
The catalogue becomes usable, because there is finally a record to attach it to.

The price is a breaking change to the snapshot wire format and a data migration
that must invent frontend models: any model with two or more backends today —
`bge-m3` across `.245` and `.246` — becomes two provider models plus a generated
frontend model, or its callers break.

`migrations/0028`'s `policy` column is stranded by this and must move (ADR 0003
territory), and per-provider names mean a client can no longer address a provider
model by a globally unique name, which forces the question ADR 0002 answers.
