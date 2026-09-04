# Provider decomposition and the provider-model rename

Status: open
Created: 2026-09-04
Milestone: providers-become-records
Issue: #7

## Description

Six columns on `model_backends` describe the endpoint, not the model:
`api_base`, `upstream_api_key`, `protocol`, `auth_header`, `auth_scheme`
(`migrations/0001_init.sql:62-72`, extended by `0013`). They move to a real
`providers` table. `model_backends` folds away; `upstream_model` and
`default_max_tokens` move onto the model row, which gains `provider_id`.

The invariant this establishes: **a provider model belongs to exactly one
provider.** Two providers serving the same model are two provider models, not
one model with two backends. Balancing between them becomes a frontend-model
concern, which is the next epic.

Names follow: with two Sparks both exposing `bge-m3`, uniqueness has to be per
provider, leaving frontend models as the only global namespace.

The rename rides the same change. "Backend model" invited exactly the mistake
being corrected here — a model with a list of backends under it — so it becomes
"provider model" in UI, docs, API and schema, with `virtual_models` renamed to
`frontend_models` so the table stops disagreeing with the screen. Doing it here
costs nothing: moving `api_base` and the credential already breaks the snapshot
wire format. Doing it later is a second breaking change for a cosmetic reason.

This is one unit of value because none of it is separable — the table split, the
one-provider invariant, the name scoping and the wire format all change
together, and a database can only be on one side of it.
