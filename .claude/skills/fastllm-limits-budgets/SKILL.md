---
name: fastllm-limits-budgets
description: Inspect and reconcile FastLLM spend and rate limits — read global budgets and limits, trigger limit reconciliation across replicas, and sync provider prices. Use when a caller is being rate-limited or refused for budget, when spend figures look wrong, or after changing provider pricing. Not for setting one principal's cap (fastllm-principals).
---

# FastLLM limits budgets

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
| `GET` | `/admin/budgets` | Read budgets | — |
| `GET` | `/admin/limits` | Read limits | — |
| `POST` | `/admin/prices/sync` | Create prices sync | `source`*, `overwrite`*, `dry_run`* |
| `POST` | `/limits/reconcile` | Exchange locally observed counts for a fresh rate-limit allowance. Proxy token only | `replica_id`, `counts` |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**Cost is computed at ingest from the price at the time of the request, and
stored.** Deriving it on read would let a later price change rewrite history.
`prices/sync` therefore affects future rows, not past ones.

**The provider's own reported cost wins where present.** It already accounts for
cache discounts and for a routed alias serving a different model; the configured
price is the fallback, not the source.

**A refusal and an upstream error are different diagnoses.** Budget and
rate-limit refusals are recorded distinctly from backend failures so they cannot
average into one misleading error rate.
