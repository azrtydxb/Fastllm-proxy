# Bind targets by name, late-resolved

Status: done 2026-09-04
Created: 2026-09-04
Epic: frontend-models-survive-their-targets
Sprint: sprint-10

## Description

As an operator who swaps a model out and back, my frontend model reattaches by itself.

A target holding a foreign key is cascaded away when the provider model is deleted: the frontend silently forgets what it wanted, with no record of what it pointed at. Binding to provider and exposed name means the same swap run again restores routing without touching the frontend.

## Acceptance criteria

- [x] Frontend model defaults and rule targets can bind to (provider, exposed name)
- [x] Deleting a provider model leaves the target in place, unresolved
- [x] Re-registering the same provider and name reattaches routing with no manual step
- [x] Existing id-bound targets keep working, or are migrated with their behaviour unchanged
- [x] No cleanup path touches frontend models, rules, targets, grants, keys or budgets — pinned by a test
- [x] Verified on a Spark: swap a model out and back, confirm routing resumes untouched

## Evidence

- `frontend_model_defaults` and `rule_targets` carry
  `target_provider_name`/`target_model_name`, and the snapshot reads the name
  rather than joining through the id (migration 0036).
- Deleting a provider model leaves the target in place, unresolved — verified
  on a clone of the live database: deleting `bge-m3@192.168.10.245:8890` left
  `embed`'s target row with its name and `bound = false`, where the old
  `ON DELETE CASCADE` removed the row entirely.
- Re-registering the same name on the same provider reattaches routing with no
  manual step, because resolution is by name at snapshot-build time.
- Existing id-bound targets were migrated with their behaviour unchanged; the
  backfill derives the name from what each target pointed at.
- Every write path records the name: both admin routes and the importer.
