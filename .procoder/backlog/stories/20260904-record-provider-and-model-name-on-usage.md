# Record the provider and model name on usage events

Status: open
Created: 2026-09-04
Epic: usage-survives-model-churn
Sprint: sprint-7

## Description

As someone reading a spend report, last week's inference is still there after this week's model swap.

`usage_events.model_id` is `ON DELETE CASCADE`, which was reasonable while models were deleted rarely and by hand. Under a lease it is not: deleting an expired provider model erases its billing history with no error.

Denormalising the name also fixes something the current schema cannot express — with per-provider names and regular swaps, one name refers to different things over time, so an id-only record is ambiguous before anything is deleted.

## Acceptance criteria

- [ ] `usage_events` records provider name and model name as text at ingest
- [ ] The FK is nullable and `ON DELETE SET NULL`
- [ ] Deleting a provider model leaves its usage rows intact and still reportable by name
- [ ] Existing rows are backfilled from current names by the migration
- [ ] Usage and spend reports read the recorded names, and a name reused for something else does not merge the two periods
- [ ] The hourly rollup is keyed by the recorded name, not the model id — a
      NULL id in a NOT NULL primary-key column would fail the whole retention
      batch, not merely lose a name
- [ ] Verified on kw: record usage, delete the model, the report still shows it

## Evidence
