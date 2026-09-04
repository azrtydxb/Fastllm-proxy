---
name: fastllm-operate
description: Operate and diagnose the FastLLM deployment on the kw Kubernetes cluster — check whether it is running, bring it back after a shutdown, deploy manifest changes, and work out why the gateway is unreachable, returning 503, or serving stale policy. Use for "is fastllm up", pods not starting, ImagePullBackOff, empty service endpoints, a missing Postgres pod, or backends out of rotation. Not for API calls against a healthy instance (fastllm-routing, fastllm-gateway and friends).
---

# Operating FastLLM on kw

## Topology

| | |
|---|---|
| Namespace | `fastllm` (control plane, proxy, CNPG Postgres) |
| Operator | `fastllm-system` |
| Gateway VIP | `192.168.10.125`, alt `192.168.10.126` |
| Admin VIP | `192.168.10.129:4001` (HTTPS, self-signed — use `-k`) |
| Postgres dev VIP | `192.168.10.127:5432` |
| Registry | `192.168.10.123` (zot, in the `registry` namespace) |
| Manifests | `deploy/` — applied continuously, so they are the source of truth |

## Diagnose in dependency order

Work down. Each step is worthless until the one above it passes.

1. **Nodes schedulable** — `kubectl get nodes`. `Ready,SchedulingDisabled` means
   cordoned: existing pods keep running while nothing new can start. A blanket
   cordon across every node is a deliberate shutdown, not a fault, and it causes
   unrelated-looking failures downstream (Longhorn's driver-deployer crash-loops
   because its helper pod cannot schedule).
2. **Workloads not scaled to zero** — a clean shutdown scales deployments and
   statefulsets to 0. Recover the intended count from
   `kubectl.kubernetes.io/last-applied-configuration` rather than assuming 1.
3. **Registry up** — the image lives in-cluster. If `registry/zot` is scaled to
   0, everything else is `ImagePullBackOff` for a reason that has nothing to do
   with the workload.
4. **Postgres present** — a CNPG cluster can report `Cluster in healthy state`
   with **no pods at all** when hibernated. Check
   `kubectl -n fastllm get cluster fastllm-pg -o jsonpath='{.metadata.annotations}'`
   for `cnpg.io/hibernation: on`; clear it with
   `kubectl -n fastllm annotate cluster fastllm-pg cnpg.io/hibernation=off --overwrite`.
   Nothing in `describe` or the operator log points at this.
5. **Image matches the schema** — `migration N was previously applied but is
   missing in the resolved migrations` means the deployed image is *older* than
   the database. Compare the manifest's tag against `migrations/`.
6. **Service endpoints non-empty** — `kubectl -n fastllm get endpoints`. A stale
   selector (for example a Helm-era four-label selector against single-label
   pods) leaves a Service with `<none>` and the proxy reporting
   `No route to host` at the control plane.
7. **Backends in rotation** — the proxy logs `backend healthy, back in rotation`
   and `backend failed N consecutive probes, out of rotation` by URL.

## Verify, do not assume

- Gateway `401` = healthy and rejecting an unauthenticated request. That is a
  successful smoke test.
- Gateway `/health` `503` = running but has no usable snapshot.
- `curl` `000` = nothing listening; that is the real failure.
- A Deployment's `spec.selector` is **immutable**. Changing labels means
  deleting and recreating it, and the Service selector must be updated to match
  or endpoints go empty.

## Do not

- Do not undo a blanket cordon or scale-up stateful workloads without asking —
  a full shutdown is usually deliberate, and databases starting cold at once is
  not free.
- Do not edit an applied migration. `sqlx` checksums them and refuses to start.
- Do not write to Postgres to make a config change when an admin session is
  available; SQL writes bypass the audit trail.
