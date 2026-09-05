# Record the provider and model name on usage events

Status: done 2026-09-04
Created: 2026-09-04
Epic: usage-survives-model-churn
Sprint: sprint-7

## Description

As someone reading a spend report, last week's inference is still there after this week's model swap.

`usage_events.model_id` is `ON DELETE CASCADE`, which was reasonable while models were deleted rarely and by hand. Under a lease it is not: deleting an expired provider model erases its billing history with no error.

Denormalising the name also fixes something the current schema cannot express — with per-provider names and regular swaps, one name refers to different things over time, so an id-only record is ambiguous before anything is deleted.

## Acceptance criteria

- [x] `usage_events` records provider name and model name as text at ingest
- [x] The FK is nullable and `ON DELETE SET NULL`
- [x] Deleting a provider model leaves its usage rows intact and still reportable by name
- [x] Existing rows are backfilled from current names by the migration
- [x] Usage and spend reports read the recorded names, and a name reused for something else does not merge the two periods
- [x] The hourly rollup is keyed by the recorded name, not the model id — a
      NULL id in a NOT NULL primary-key column would fail the whole retention
      batch, not merely lose a name
- [x] Verified on kw: record usage, delete the model, the report still shows it

## Evidence

- `usage_events` records `model_name` and `provider_name` at ingest, FK is
  nullable `ON DELETE SET NULL` — `migrations/0031`, applied to kw as
  migration 31.
- Deleting a model leaves its usage intact: on a clone of the live database,
  deleting `gpt-5` left all 4 of its usage rows, `model_id` cleared to NULL and
  `model_name`/`provider_name` preserved.
- Existing rows backfilled from current names by the migration.
- Reports read the recorded name: `group_by=model` uses
  `coalesce(u.model_name, m.name)`, so a deleted model keeps its own bucket
  instead of collapsing into one nameless row.
- The hourly rollup is keyed by name, not id. This was the sharper half —
  `usage_rollup_hourly.model_id` is `NOT NULL` _and_ in the primary key, so a
  deleted model would have failed the entire retention batch rather than losing
  a name. PK on kw is now `(hour, model_name, principal_id)`.
- Verified on kw with live traffic: a request shows
  `requested_model = bge-m3`, `model_name = bge-m3@192.168.10.245:8890`,
  `provider_name = 192.168.10.245:8890`.
- Pinned by `deleting_a_model_keeps_the_usage_it_was_billed_for` in
  `tests/usage_retention.rs`.
