# Rename to provider models and frontend models

Status: open
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-2

## Description

As someone reading the schema or the screen, one word means one thing.

"Backend model" invited exactly the mistake this epic corrects — a model with a list of backends under it. It becomes "provider model" in UI, docs, API and schema, and `virtual_models` becomes `frontend_models` so the table stops disagreeing with the screen that already says "Frontend models".

Measured: 35 `FROM`/`JOIN`/`INTO`/`UPDATE models` and 91 `model_id` in `src/`, 107 `virtual_model*` in `src/` and `migrations/`, 9 prose mentions of "backend model" and no identifiers. It rides this epic's breaking change rather than adding a second one.

Per CLAUDE.md the docs change in the same commit, not afterwards.

## Acceptance criteria

- [ ] `models` → `provider_models`, `model_id` → `provider_model_id`, `virtual_models` → `frontend_models`, via `ALTER TABLE … RENAME` in a new migration
- [ ] No applied migration is edited; `sqlx` starts clean against the kw database
- [ ] `/admin/models` → `/admin/provider-models`, `/admin/virtual-models` → `/admin/frontend-models`
- [ ] `openapi.json` regenerated and the generated skill endpoint tables regenerated (`scripts/gen-claude-skills.py --check` passes)
- [ ] README, `docs/architecture.md`, `docs/api.md`, `deploy/*.yaml` comments updated in the same commit
- [ ] No occurrence of "backend model" remains outside a changelog entry

## Evidence
