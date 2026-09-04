# Migrate multi-backend models without breaking callers

Status: open
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-2

## Description

As a client calling `bge-m3` today, my requests keep working after the migration.

`bge-m3` is currently one model with two backends (`.245:8890` and `.246:8890`) and clients name it directly. After the split it is two provider models, so something has to hold the two together under the old name — an auto-created frontend model, inheriting the old `models.policy`. The name is available precisely because provider-model names went provider-scoped.

Single-backend models — the three OpenRouter ones — need nothing.

Grants are the half that is easy to miss, and 0029 missed it. `proxy.rs` authorises against the
resolved concrete model, so renaming one revokes every grant naming it. The
first real request after this migration reached kw came back `403
model_access_denied` for `bge-m3@192.168.10.245:8890`, a model the caller had
never heard of, and two live roles had lost access with nothing reporting it.

Fixed forward in migration 0030 rather than by correcting 0029, which had
already been applied: `sqlx` checksums migrations and refuses to start when an
applied one changes. 0030 recovers the split from the shape 0029 left behind —
a frontend model whose target is named `<frontend name>@<something>` — and
extends every grant on the original name to each name it was split into.

## Acceptance criteria

- [ ] Every model with two or more backends gains a frontend model of the same name balancing across the split provider models
- [ ] The generated frontend model inherits the old `models.policy`
- [ ] Single-backend models gain nothing
- [ ] A frontend model that already pointed at a split model still reaches every
      provider it used to — the split must not quietly halve its capacity
- [ ] A request to `bge-m3` on kw succeeds before and after the migration with the same client config
- [ ] Every grant on the original name reaches each name it was split into — a
      caller holding `model:invoke` on the old name can still make the same
      request afterwards
- [ ] The migration is idempotent, or refuses to run twice, and is exercised against a copy of the kw database

## Evidence
