# Stop dropping usage for unknown models

Status: open
Created: 2026-09-04
Epic: usage-survives-model-churn
Sprint: sprint-7

## Description

As an operator, traffic that already happened is recorded even if its model has since gone.

`POST /usage` silently drops batch rows naming a model id that no longer exists — the same judgement as the cascade, applied at ingest. A proxy flushing a batch for a model that expired mid-flush loses those rows, which under churn is the normal shape of a swap rather than an edge case.

## Acceptance criteria

- [ ] A usage batch naming a model id that no longer exists is recorded, not dropped
- [ ] The comment in `migrations/0005` and any code comment stating the old behaviour is corrected in the same commit
- [ ] A batch naming a principal that no longer exists is handled by the same rule, or the difference is justified in the code
- [ ] Verified on kw: delete a provider model mid-flight and confirm the in-flight usage still lands

## Evidence
