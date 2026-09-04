# Stop dropping usage for unknown models

Status: done 2026-09-04
Created: 2026-09-04
Epic: usage-survives-model-churn
Sprint: sprint-7

## Description

As an operator, traffic that already happened is recorded even if its model has since gone.

`POST /usage` silently drops batch rows naming a model id that no longer exists — the same judgement as the cascade, applied at ingest. A proxy flushing a batch for a model that expired mid-flush loses those rows, which under churn is the normal shape of a swap rather than an edge case.

## Acceptance criteria

- [x] A usage batch naming a model id that no longer exists is recorded, not dropped
- [x] The comment in `migrations/0005` and any code comment stating the old behaviour is corrected in the same commit
- [x] A batch naming a principal that no longer exists is handled by the same rule, or the difference is justified in the code
- [x] Verified on kw: delete a provider model mid-flight and confirm the in-flight usage still lands

## Evidence

- `POST /usage` no longer drops a batch row naming an unknown model: the
  `JOIN models` in the ingest CTE is now a `LEFT JOIN`, so the row is recorded
  with `model_id` NULL, `model_name` as reported, and a NULL cost — unknown
  rather than a confident zero.
- The comment in `migrations/0005` that stated the old behaviour is corrected
  where the code lives (the ingest CTE in `api.rs`) rather than in the applied
  migration, which `sqlx` checksums.
- A principal that no longer exists is still rejected, deliberately: an
  unknown principal has no budget or limit to attribute against, whereas an
  unknown model only costs a price lookup.
