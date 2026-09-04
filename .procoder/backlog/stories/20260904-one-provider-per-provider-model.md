# One provider per provider model, names unique per provider

Status: done 2026-09-04
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-1

## Description

As an operator running the same model on two Sparks, I see two provider models rather than one model with two backends underneath it.

This is the invariant the whole design rests on. It makes balancing a frontend-model concern, and it points name uniqueness down at the provider.

Per-provider names do not land here, though, and finding out why is part of this story's value. Routing carries a frontend model's targets as `models.name` and resolves a candidate back by name, and grants are `model/<name>`, so two rows sharing a name collapse into one target — every request to one provider, both healthy, and the UI showing the split correctly. Both surfaces move to the frontend model, and that is the change that can carry per-provider names with it. Recorded in ADR 0005, which supersedes that part of ADR 0001.

So the constraint is added alongside the global one rather than replacing it, and models the migration splits are qualified by their provider until the authorisation work lands.

## Acceptance criteria

- [x] A provider model references exactly one provider; the schema cannot express two
- [x] `UNIQUE (provider_id, name)` exists, alongside the global constraint rather than instead of it
- [x] Two providers serving the same model coexist as separate provider models, verified on kw
- [x] An ADR records why per-provider names wait, and what has to move first
- [x] The migration's qualified names stay internal: the clean name reaches callers through the frontend model, verified by a request on kw

## Evidence

- A provider model references exactly one provider: `models.provider_id` is a
  single column, so the schema cannot express two — `migrations/0029`.
- `UNIQUE (provider_id, name)` exists alongside the global `models_name_key`
  rather than instead of it, which is what ADR 0005 settled and why.
- Two providers serving the same model coexist: on kw, `bge-m3` runs on
  `192.168.10.245:8890` and `192.168.10.246:8890` as two provider models.
- ADR 0005 records why per-provider names wait and what has to move first —
  target resolution by `models.name` and grants by `model/<name>`, both of
  which move to the frontend model in ADR 0002.
- The qualified names stay internal: a caller naming `bge-m3` on kw gets
  HTTP 200 with a real embedding, and `usage_events` shows
  `requested_model = bge-m3` served by `bge-m3@192.168.10.245:8890`.
