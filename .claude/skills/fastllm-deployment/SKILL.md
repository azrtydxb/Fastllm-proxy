---
name: fastllm-deployment
description: Inspect and control the running FastLLM deployment — read effective configuration and deployment settings, force a snapshot rebuild, fetch the snapshot the proxies consume, and check liveness and readiness. Use when a configuration change has not taken effect, when a proxy is serving stale policy, or to confirm what a running instance actually believes.
---

# FastLLM deployment

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
| `GET` | `/admin/config` | What this process was started with, and who is asking | — |
| `GET` | `/admin/deployment` | The FastllmProxy resource running this deployment, and its status | — |
| `PATCH` | `/admin/deployment` | Change the shape of this deployment: image, replicas, policy, autoscaling | — |
| `POST` | `/admin/snapshot/rebuild` | Rebuild and republish the snapshot now | — |
| `GET` | `/docs` | Swagger UI over the spec. The page pulls its bundle from a CDN, so an air-gapped deployment gets an empty page while /openapi.json still works | — |
| `GET` | `/health` | Per-backend health, in-flight and error counts. Unauthenticated - exposes backend addresses | — |
| `POST` | `/health-report` | Per-replica backend health. Proxy token only | — |
| `GET` | `/healthz` | Liveness. Unauthenticated and does no database work, so probing it often costs nothing | — |
| `GET` | `/openapi.json` | This document. Unauthenticated: a spec you need a session to read is one nobody generates a client from | — |
| `GET` | `/snapshot` | The flattened routing table a proxy replica polls. Proxy token only - returns decrypted upstream credentials | — |
| `POST` | `/usage` | Batched usage reporting from a proxy replica. Proxy token only | `events` |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**Config changes reach proxies on their snapshot poll, not instantly.** If a
change appears in `/admin/*` but not in behaviour, check the proxy actually
refreshed before assuming the change was wrong.

**`/health` 503 on the gateway means no usable snapshot**, not a dead process. A
`401` from the gateway means it is healthy and rejecting an unauthenticated
request — that is a successful smoke test, not a failure.

**`AppState::apply_snapshot` is the single write path**, and it rebuilds the
routing registry in the same call so the two cannot diverge. Never write the
snapshot cell directly.

**The request path performs no I/O.** `tests/no_io_on_hot_path.rs` guards this
and must be extended whenever new work lands there.
