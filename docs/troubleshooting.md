# Troubleshooting

Symptoms people actually hit, and what each one means. Most entries here are
failures that happened on a real deployment rather than ones imagined for the
page — where a message turned out to be misleading, that is called out, because
a wrong explanation costs more than no explanation.

## Requests

### `401` with a key you just created

The key exists in the database but the proxy has not seen it yet. Keys reach
the data plane in the snapshot, which each replica polls on `--config-poll`
(5s by default), so there is a window of a few seconds after minting.

If it persists: the key may be revoked (`GET /admin/keys` shows `disabled`),
expired (`expires_at` — and note the UI's create form defaults to **90 days**,
not never), or you are sending it to the admin port instead of the data plane.

### `403 model_access_denied`

The key is valid; the principal behind it holds no `model:invoke` grant for
that model. Authentication and authorisation are separate, and a 403 rather
than a 401 is the gateway saying so.

```bash
curl -sk -b /tmp/ck https://host:4001/admin/principals   # which roles it holds
curl -sk -b /tmp/ck https://host:4001/admin/roles        # what those roles grant
```

Grants are per model: `model:invoke` on `model/<name>`, or `model/*` for all,
and the name is a **frontend model's** — that is what a client asks for and
therefore what is granted.

A grant on a frontend model covers the whole chain it routes to. Adding a
target to it extends the reach of everyone holding it, which is acceptable only
because editing one needs `config:write`, itself enough to grant any model
outright.

### `404` for a provider model

Provider models are inventory, not names a client may use. Ask for the frontend
model in front of it — `GET /v1/models` lists exactly what this key can name.

### `404 no route for POST /v1/...`

That path is not proxied. Twelve `POST` endpoints carrying a `model` are; the
stateful job APIs are not, and [integrations.md](integrations.md) explains why.

### `429`, and `x-ratelimit-*` headers you did not configure

Rate limits are per principal. `GET /admin/limits` shows who has one.
`Retry-After` is in whole seconds and is honest — the bucket really does refill.

### `402 Payment Required`

A budget window is exhausted. Not a rate limit: waiting does not help until the
window rolls over or someone raises the cap. `GET /admin/budgets`.

### `502 upstream_unavailable`

No backend in the chain could be reached. Check `GET /health` on the data plane
for per-backend health, or the **Fleet** screen, which keeps replicas separate —
if one replica sees a backend as down and the others do not, that is a
partition rather than a dead backend, and merging them would hide it.

### Requests succeed but the model returns empty `content`

Reasoning models put their output in `reasoning_content` until they finish
thinking. A small `max_tokens` truncates the reasoning before any content
appears, and `finish_reason` will say `length`. This is also the most common
cause of **"the model will not call tools"**: a tool call costs ~100 tokens of
reasoning first, so a tight ceiling looks exactly like a model that ignores
tools. Raise `max_tokens`, or disable thinking:

```json
{ "chat_template_kwargs": { "enable_thinking": false } }
```

## Usage, spend and charts

### Usage and spend are empty

Two eras here. Before the accounting change, usage was recorded **only** for
principals with a budget or a tokens-per-minute limit — so a deployment that
enforced nothing recorded nothing. Every request is recorded now, for any
authenticated caller.

If it is still empty: check that requests are reaching a backend at all
(refusals are recorded separately), and that the control plane is receiving
reports — `fastllm_usage_reports_dropped_total` on the proxy's `/metrics` is
non-zero if the queue to the control plane is backing up.

### Spend says `—` or `unpriced`

The models have no prices. A request against an unpriced model contributes
nothing to a total and is counted as `unpriced_requests` rather than as zero
cost, so a spend figure never quietly understates. Set prices with
`PATCH /admin/provider-models/{id}` or the edit form. A self-hosted model legitimately
has no price; a hosted one should have.

### The chart says "a control plane older than the accounting change does not serve this"

Take that message with suspicion — it asserts a cause it has not checked.
It appears whenever `GET /admin/timeseries` fails for _any_ reason, including
a 500. Check the endpoint directly before believing it:

```bash
curl -sk -b /tmp/ck 'https://host:4001/admin/timeseries?bucket=3600'
```

If that returns 500, the control-plane logs have the real reason.

### A chart is empty for a window you know had traffic

Empty buckets are returned as explicit zeros, so an empty chart means no rows,
not a missing series. Usage older than the retention window (90 days) has been
folded into hourly rollups — the counts survive, but **rolled-up buckets carry
no latency**, because percentiles do not merge. A latency line that stops
partway back is that boundary, not a gap in traffic.

## Admin plane

### The browser warns about the certificate

The admin API is served with a certificate from a private CA, which no OS
trust store knows. Verifying clients need the CA:

```bash
kubectl -n fastllm get secret fastllm-control-tls -o jsonpath='{.data.ca\.crt}' \
  | base64 -d > ca.crt
curl --cacert ca.crt https://host:4001/healthz
```

For a browser, trust that CA on the machine. Do not habitually click through
certificate warnings on an admin plane.

### `migration N was previously applied but is missing in the resolved migrations`

The binary is **older** than the database schema. Usually a rollback, or a
manifest whose image pin drifted behind what is running. `sqlx` refuses rather
than running against a schema it does not understand, which is the safe
failure. Deploy the newer image.

### A write succeeded but nothing changed

`GET /admin/health` reports `snapshot_rebuild_failures`. A write commits and
_then_ the snapshot is rebuilt; if the rebuild fails, the database and the
published configuration have diverged and will stay that way until a later
rebuild succeeds. This is also a webhook event, if one is configured.

### One replica behaves differently from the others

Compare `snapshot_version` per replica on the **Fleet** screen. A replica on an
older snapshot answers `/health` with `ok` and misbehaves only on whatever
changed — most often a key it has never seen.

## Backends

### A backend keeps being marked unhealthy

Health is consecutive-failure based (`unhealthy_after`, default 2). A backend
that is up but slow enough to exceed `--health-timeout` looks identical to one
that is down. For long-loading engines, raise the timeout rather than the
failure count.

### Two nodes serving the same model behave differently

Check they are running the same engine build. A load-balanced pool whose
members differ is a pool that fails intermittently and blames the router —
pin by digest, not by a floating tag.

### Embeddings work but their tokens are not counted

Fixed. Any non-streaming response larger than the 8 KiB tail buffer used to
lose its usage, which for a 22 KB embeddings response was every one of them.
If you see it on an older build, that is the cause.
