# One provider per provider model, names unique per provider

Status: open
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-1

## Description

As an operator running the same model on two Sparks, I see two provider models rather than one model with two backends underneath it.

This is the invariant the whole design rests on. It makes balancing a frontend-model concern, and it points name uniqueness down at the provider.

Per-provider names do not land here, though, and finding out why is part of this story's value. Routing carries a frontend model's targets as `models.name` and resolves a candidate back by name, and grants are `model/<name>`, so two rows sharing a name collapse into one target — every request to one provider, both healthy, and the UI showing the split correctly. Both surfaces move to the frontend model, and that is the change that can carry per-provider names with it. Recorded in ADR 0005, which supersedes that part of ADR 0001.

So the constraint is added alongside the global one rather than replacing it, and models the migration splits are qualified by their provider until the authorisation work lands.

## Acceptance criteria

- [ ] A provider model references exactly one provider; the schema cannot express two
- [ ] `UNIQUE (provider_id, name)` exists, alongside the global constraint rather than instead of it
- [ ] Two providers serving the same model coexist as separate provider models, verified on kw
- [ ] An ADR records why per-provider names wait, and what has to move first
- [ ] The migration's qualified names stay internal: the clean name reaches callers through the frontend model, verified by a request on kw

## Evidence
