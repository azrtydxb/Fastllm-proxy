# Rename to provider models and frontend models

Status: done 2026-09-04
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-2

## Description

As someone reading the schema or the screen, one word means one thing.

"Backend model" invited exactly the mistake this epic corrects — a model with a list of backends under it. It becomes "provider model" in UI, docs, API and schema, and `virtual_models` becomes `frontend_models` so the table stops disagreeing with the screen that already says "Frontend models".

Measured: 35 `FROM`/`JOIN`/`INTO`/`UPDATE models` and 91 `model_id` in `src/`, 107 `virtual_model*` in `src/` and `migrations/`, 9 prose mentions of "backend model" and no identifiers. It rides this epic's breaking change rather than adding a second one.

Per CLAUDE.md the docs change in the same commit, not afterwards.

## Acceptance criteria

- [x] `models` → `provider_models`, `model_id` → `provider_model_id`, `virtual_models` → `frontend_models`, via `ALTER TABLE … RENAME` in a new migration
- [x] No applied migration is edited; `sqlx` starts clean against the kw database
- [x] `/admin/models` → `/admin/provider-models`, `/admin/virtual-models` → `/admin/frontend-models`
- [x] `openapi.json` regenerated and the generated skill endpoint tables regenerated (`scripts/gen-claude-skills.py --check` passes)
- [x] README, `docs/architecture.md`, `docs/api.md`, `deploy/*.yaml` comments updated in the same commit
- [x] No occurrence of "backend model" remains outside a changelog entry

## Evidence

- `provider_models`, `frontend_models`, `frontend_model_defaults`, and the
  `provider_model_id` / `frontend_model_id` columns across four tables —
  `migrations/0033`, applied to kw as migration 33. Indexes renamed too, since
  `models_pkey` sitting on `provider_models` reads as half-finished.
- No applied migration was edited; `sqlx` started clean against kw and 0033
  reports `success = t`.
- Routes: `/admin/provider-models` and `/admin/frontend-models` return 200 on
  kw, `/admin/models` and `/admin/virtual-models` return 404. The break is the
  point — it rides 0029's breaking change rather than adding a second one later
  for a cosmetic reason.
- `openapi.json` updated and the generated skill tables regenerated;
  `scripts/gen-claude-skills.py --check` exits 0, which needed the domain
  patterns updating for the new paths.
- README, `docs/` (23 files), `deploy/` comments and the skills updated in the
  same change. A route renamed in code and not in the docs is a 404 with a
  promise behind it.
- No occurrence of "backend model" or `virtual_model` remains in `src/`,
  `tests/`, `web/src` or the docs.
- The gateway is unaffected: an embeddings request through `embed` on kw
  returns 200 after the rename.
- SQL replacements were anchored on the preceding keyword (`FROM`/`JOIN`/
  `INTO`/`UPDATE`/`TABLE`) rather than replacing `models` globally, so
  `/v1/models`, `model_list` and `Snapshot.models` were never touched.
