---
name: fastllm-observability
description: Read what FastLLM has been doing — usage records, time-series aggregates, the configuration audit trail, Prometheus metrics, control-plane health, and per-replica fleet status. Use when asked how much a caller spent, what changed and who changed it, whether a backend is healthy, or to investigate an error-rate or latency question.
---

# FastLLM observability

## Auth

Admin endpoints need a **session cookie**, not a bearer token — the gateway
master key is not an admin credential.

```bash
curl -sk -c /tmp/ck -X POST https://192.168.10.129:4001/login \
  -H 'content-type: application/json' -d '{"name":"<user>","password":"<pw>"}'
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/...
```

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `GET` | `/admin/audit` | The change log, newest first, keyset-paginated | — |
| `GET` | `/admin/fleet` | What each proxy replica reports, kept per replica and never merged | — |
| `GET` | `/admin/health` | Read health | — |
| `GET` | `/admin/timeseries` | Bucketed traffic, latency and spend. Empty buckets come back as explicit zeros; latency is null where there was nothing to measure | — |
| `GET` | `/admin/usage` | Aggregate usage and spend, grouped by model, principal, virtual model or day | — |
| `GET` | `/metrics` | Prometheus text. Unauthenticated | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Facts worth knowing

**The audit trail is middleware, not hand-wired.** Everything that is not a `GET`
under `/admin/*` passes through it, so a newly added endpoint is audited before
it is written. `GET`s are deliberately not audited — auditing reads would bury
the changes in noise.

**Direct database writes produce no audit row.** If a change was made with
`psql` because no admin credential was available, the audit trail will not show
it; say so explicitly rather than letting the absence imply nothing happened.

**A usage row exists for every attributable request**, including ones whose
response carried no token counts. `usage_reported` distinguishes "consumed
nothing" from "counts unknown" — treating them alike understates consumption.

**`/admin/fleet` never averages replicas together.** Every replica losing a
backend is a dead backend; one replica losing it is a partition.
