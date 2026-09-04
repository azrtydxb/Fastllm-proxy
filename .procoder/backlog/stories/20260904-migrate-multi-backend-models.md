# Migrate multi-backend models without breaking callers

Status: open
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-2

## Description

As a client calling `bge-m3` today, my requests keep working after the migration.

`bge-m3` is currently one model with two backends (`.245:8890` and `.246:8890`) and clients name it directly. After the split it is two provider models, so something has to hold the two together under the old name — an auto-created frontend model, inheriting the old `models.policy`. The name is available precisely because provider-model names went provider-scoped.

Single-backend models — the three OpenRouter ones — need nothing.

## Acceptance criteria

- [ ] Every model with two or more backends gains a frontend model of the same name balancing across the split provider models
- [ ] The generated frontend model inherits the old `models.policy`
- [ ] Single-backend models gain nothing
- [ ] A request to `bge-m3` on kw succeeds before and after the migration with the same client config
- [ ] The migration is idempotent, or refuses to run twice, and is exercised against a copy of the kw database

## Evidence
