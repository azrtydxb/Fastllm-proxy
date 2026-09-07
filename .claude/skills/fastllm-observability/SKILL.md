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
| `GET` | `/admin/nodes` | The hosts registering their own endpoints, rolled up per node. An agent is not a row: it is a `node` several dynamic providers share, and its lease is what says it is alive | — |
| `GET` | `/admin/timeseries` | Bucketed traffic, latency and spend. Empty buckets come back as explicit zeros; latency is null where there was nothing to measure | — |
| `GET` | `/admin/usage` | Aggregate usage and spend, grouped by model, principal, frontend model or day | — |
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

**`snapshot_version` spread is not a measure of staleness.** A version is the
microsecond the control plane built that snapshot, and it republishes only when
the content changed — so the gap between two consecutive versions is the time
between two real config changes, not any replica's lag. A replica one version
behind can show a gap of a second or of a minute at identical health.

Judge it by time instead. Proxies poll every `config_poll_seconds` and report
health every `health_report_interval_seconds` (both in `GET /admin/config`), so
a replica is stuck only if the newest snapshot has been available for longer
than their sum — or if it has been behind across several samples spanning that
long. The second test is the one that works on a busy gateway, where
`Budget.tokens_used` being part of the snapshot means traffic alone republishes
it every few seconds and nothing is ever old. One `GET /admin/fleet` cannot
distinguish a stuck replica from a converging one there; take a few, spaced.

Reading any spread at all as a fault reports a healthy fleet as split after
every change.
