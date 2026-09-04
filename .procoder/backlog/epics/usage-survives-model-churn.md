# Usage survives model churn

Status: done 2026-09-04
Created: 2026-09-04
Milestone: self-registering-hosts
Issue: #9

## Description

`usage_events.model_id` is `ON DELETE CASCADE`
(`migrations/0005_usage_events.sql:17`), on the stated grounds that "a usage row
for a principal or model that no longer exists describes nothing an operator can
act on". The same reasoning is why `POST /usage` silently drops batch rows
naming a model id that no longer exists.

Both hold only while models are deleted rarely and deliberately by a human.
Dynamic registration breaks that premise on purpose.

Swap the 27B off a Spark on Friday and that week's local inference disappears
from usage and spend, with no error — the cascade does what it was told. A proxy
flushing a batch for a model that expired mid-batch loses those rows too, which
under churn is not an edge case but the normal shape of a swap.

Recording the provider and model name as text at ingest, with a nullable FK,
fixes both. It also fixes something the current schema cannot express at all:
with per-provider names and regular swapping, one name refers to different
things over time, so an id-only record of what was billed is ambiguous before
anything is even deleted.

Small, independently correct, and worth doing even if the registration service
is never built.
