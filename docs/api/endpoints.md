# The endpoints clients call

What the gateway serves on `:4000`, what it deliberately does not, and
the headers and retry behaviour that come with each.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `POST /v1/chat/completions` | Proxied byte-for-byte. Also `/completions`, `/responses`, `/embeddings`, `/rerank`, `/score`, `/audio/transcriptions`, `/audio/translations`, `/audio/speech`, `/images/generations`, `/images/edits`, `/moderations` |
| `GET /v1/models` | Aggregated across every pool, **filtered to what the calling key may invoke**. A virtual model is listed when the caller can invoke any model it routes to. Clients build model pickers from this, and offering names that 403 on selection is a defect the authorisation being correct does not excuse |
| `GET /health` | Per-backend health, in-flight, request and error counts, plus `snapshot_version` and the key count for the configuration this process is serving. No auth required. Exposes backend addresses — keep it off the public interface |
| `GET /metrics` | Prometheus text, including `fastllm_snapshot_version`. No auth required |
| `/admin/*` | `--role all`/`control` only. Gated by a session cookie (`POST /login`), not `--proxy-token` — see the table below and "Admin authentication" underneath it |
| `POST /login` / `POST /logout` | `--role all`/`control` only. Argon2id password check; sets/clears the `fastllm_session` cookie every other `/admin/*` route requires |
| `/`, `/ui/*` (management UI) | `--role all`/`control` only. The embedded SPA — see "Management UI" below |
| `GET /snapshot` | `--role all`/`control` only. What `--role proxy` polls in `Http` mode; gated by `--proxy-token` |
| `POST /usage` | `--role all`/`control` only. Batched usage reporting from `--role proxy` (see "TLS and the reverse channel" below); gated by the same `--proxy-token` as `/snapshot` |
| `POST /limits/reconcile` | `--role all`/`control` only. Rate-limit count reporting from `--role proxy` (see "Rate limits" below); gated by the same `--proxy-token` |

## Endpoints, and what is not one

Twelve `POST` endpoints are proxied. All of them take the same path: read
`model` from the body, authorise it, route it, forward the bytes. Nothing on
that list is parsed on the way back, so adding one costs a line — which is why
`/responses`, `/audio/speech`, `/images/*` and `/moderations` are there.

A **native** (`anthropic`/`gemini`) backend answers `501` for everything except
`/chat/completions`, because only chat has a translation. That gate is what
makes adding a passthrough endpoint safe: a native backend refuses it clearly
instead of being handed a body it cannot read.

**What is deliberately absent, and why it is not a line of config.** The
stateful job APIs — `/batches`, `/files`, `/fine_tuning` — are not endpoints so
much as small databases. Creating a job is a `POST` with a `model` in it, which
would work; *retrieving* one is a `GET /v1/batches/{id}` with no model and no
body, so there is nothing to route on. Serving them means remembering which
backend owns which job id, which is durable state on the request path — the one
thing this proxy is built not to have. They need a design, not a suffix.

## Response cache

Off unless a model asks for it:

```bash
-d '{"name":"embeddings","cache_ttl_seconds":300}'
```

An identical request to that model — same resolved model, same body — is
answered from memory without touching the provider. Responses carry
`x-fastllm-cache: hit` or `miss`, because a caller measuring latency deserves
to know why one request took a microsecond and the next took a second.

Opt-in per model rather than global, because caching changes semantics: two
identical requests at `temperature > 0` are supposed to be able to differ. A
deployment that sets nothing pays nothing, not even the hash — that is only
computed once a model is known to have caching on.

**Non-streaming 2xx responses only.** Caching a stream would mean buffering the
whole response before any of it reached the client, turning the one path this
proxy exists to keep incremental into a batch operation. Errors are never
cached: a 429 is a statement about *now*, and serving it from cache would keep
a provider's bad minute alive long after it ended. The natural fit is
embeddings and short completions, which are the requests that repeat.

The cache is **per process**, bounded by `--cache-max-entries` and
`--cache-max-bytes` (both matter: a thousand embedding responses is nothing and
a thousand completions is hundreds of megabytes). A shared cache would mean a
network call, and the request path performs no I/O — a lower hit rate across
replicas is the honest cost of that invariant.

A cache hit still counts against the caller's rate limit and budget. A cache is
a latency and cost optimisation, not a way around a quota. And the whole cache
is dropped whenever a snapshot changes, since a reconfiguration can repoint a
model at a different provider and there is no way to tell from a key which
entries are affected — a cold cache is a latency cost where a stale one is a
correctness bug.

## Rate limit headers

Every response from a principal with limits configured carries the de-facto
`x-ratelimit-*` shape, so a client that already paces itself against OpenAI
needs no new code:

```
x-ratelimit-limit-requests / x-ratelimit-remaining-requests
x-ratelimit-limit-tokens   / x-ratelimit-remaining-tokens
x-ratelimit-reset
```

Remaining is floored, not rounded — 0.6 of a request is not one a client can
spend. `x-ratelimit-reset` is seconds until the allowance is fully back; a
token bucket has no discrete window to reset, so that is the honest reading,
and a full bucket reports 0. A principal with no limits gets no headers at all,
because publishing `remaining: 0` to an unlimited caller would make a
well-behaved client back off against a limit that does not exist.

## Retries

A retry waits 25ms, then 50ms, then 100ms, plus up to 50% jitter, and doubles
that for a 429 — a provider that just said "too many requests" means it.
Bounded deliberately: the delay is paid by a client still waiting for its
answer, so this is a retry budget measured against one request's patience, not
a background job's. Jitter is keyed on the request rather than an RNG, since
the data plane has no random source in a `--no-default-features` build and all
that matters is that simultaneous retries decorrelate.
