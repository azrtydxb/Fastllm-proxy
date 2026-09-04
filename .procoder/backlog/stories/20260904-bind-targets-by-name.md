# Bind targets by name, late-resolved

Status: open
Created: 2026-09-04
Epic: frontend-models-survive-their-targets
Sprint: sprint-10

## Description

As an operator who swaps a model out and back, my frontend model reattaches by itself.

A target holding a foreign key is cascaded away when the provider model is deleted: the frontend silently forgets what it wanted, with no record of what it pointed at. Binding to provider and exposed name means the same swap run again restores routing without touching the frontend.

## Acceptance criteria

- [ ] Frontend model defaults and rule targets can bind to (provider, exposed name)
- [ ] Deleting a provider model leaves the target in place, unresolved
- [ ] Re-registering the same provider and name reattaches routing with no manual step
- [ ] Existing id-bound targets keep working, or are migrated with their behaviour unchanged
- [ ] No cleanup path touches frontend models, rules, targets, grants, keys or budgets — pinned by a test
- [ ] Verified on a Spark: swap a model out and back, confirm routing resumes untouched

## Evidence
