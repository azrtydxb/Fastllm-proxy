# Architecture

Keep this current. Per `CLAUDE.md`, a change that adds a component, a role, an
endpoint crossing a plane boundary, or alters how the snapshot moves makes
these diagrams wrong, and redrawing them is part of that change's commit.

## The two planes

One binary, three roles. The split between management and forwarding is a
runtime flag, not a deployment boundary, so the same image is a single
container in a lab and a scaled deployment in Kubernetes.

```mermaid
flowchart LR
    client([OpenAI client])

    subgraph dp["Data plane — --role proxy"]
        auth[authenticate + authorise]
        route[resolve model, evaluate rules]
        limit[rate limit + budget check]
        fwd[forward opaque bytes]
        snap[(Snapshot<br/>in memory)]
        cache[(last-known-good<br/>on disk)]
    end

    subgraph cp["Control plane — --role control"]
        admin[admin API + UI]
        build[build snapshot]
        pg[(Postgres)]
    end

    backend([vLLM / SGLang<br/>OpenAI-compatible])

    client -->|"Bearer sk-…"| auth --> route --> limit --> fwd --> backend
    auth -.reads.-> snap
    route -.reads.-> snap
    limit -.reads.-> snap

    admin --> pg
    build --> pg
    build -->|publishes| admin
    snap <-->|"GET /snapshot (TLS, ETag)"| admin
    snap -.writes.-> cache
    cache -.restores on cold start.-> snap
    fwd -->|"POST /usage (batched)"| admin
```

`--role all` runs both boxes in one process; the snapshot is handed over in
memory instead of over HTTP, and the rate-limit reconciliation machinery is
inert because one process's counters are already global.

## Why a snapshot, and why it is pre-flattened

The request path must do no I/O — the proxy's measured overhead against a real
vLLM is zero, and a database round trip per request would cost more than the
proxying itself. So every expensive question is answered once, when the
snapshot is built, never per request:

```mermaid
flowchart TD
    r[roles] --> p[permissions]
    p --> w["wildcards expanded<br/>model/* → allow_all"]
    w --> flat["Principal.allowed_models<br/>(a flat HashSet)"]
    flat --> ask["request path:<br/>one set lookup"]
```

Per request that leaves: one SHA-256 of the bearer token, one hash lookup to a
principal, an expiry comparison, one set lookup for the model, one atomic
decrement for the rate limit. No graph walk, no lock held across an await, no
allocation beyond what the body already needed.

## A request, end to end

```mermaid
sequenceDiagram
    participant C as Client
    participant P as Proxy
    participant B as Backend
    participant K as Control plane

    C->>P: POST /v1/chat/completions
    P->>P: SHA-256 → principal (401 if unknown/expired)
    P->>P: resolve model — virtual models evaluate rules here
    P->>P: authorise the RESOLVED concrete model (403 if ungranted)
    P->>P: rate limit (429) and budget (402)
    P->>B: forward, original bytes
    B-->>P: response frames
    P-->>C: same frames, never parsed
    Note over P: tail buffer mirrors the last few KB
    P->>P: at end of stream, parse once for usage
    P-)K: POST /usage (batched, fire-and-forget)
    K->>K: fold into budgets.tokens_used
```

Two decisions in that flow are load-bearing:

- **Authorisation is checked against the resolved concrete model**, never the
  virtual name. A virtual model routes access; it must never grant it, or a
  rule edit or a weighted split could hand a caller a model they were never
  granted.
- **Usage is read from a fixed-size tail buffer, parsed once at the end** —
  never per frame. The response is still forwarded as opaque bytes.

## Failure modes

| event | behaviour |
|---|---|
| control plane down, proxy warm | serves from memory; policy stops changing |
| control plane down, proxy cold | loads last-known-good from disk |
| cold start, no cache | starts, `/health` unhealthy, never crash-loops |
| snapshot invalid | keeps the previous one, logs once |
| key revoked | effective within the poll interval, ~1s |
| Postgres down | control plane serves its last built snapshot; proxies unaffected |
| usage report fails | dropped; never blocks a request |

Never crash-looping on a cold start is deliberate: under Kubernetes that would
turn a control-plane outage into a data-plane outage, which is the failure this
split exists to prevent.

## Consistency, stated honestly

- **Budgets are enforced after the fact.** A request that blows the budget
  completes; the next is refused. Counting mid-stream would mean parsing every
  frame.
- **Rate limits can overshoot by up to one reconciliation window** during a
  sharp spike, because replicas enforce locally and reconcile periodically
  rather than sharing a counter on the request path.
- **Policy changes propagate within one snapshot poll**, not instantly.
