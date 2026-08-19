# Logs, metrics and traces

What this process tells you about itself, and which of it is worth an alert.

## Logs

`--log-format text` (the default) is human-readable; `--log-format json`
(`FASTLLM_LOG_FORMAT=json`) emits one JSON object per line with the event's
fields at the top level rather than nested, which is what a collector indexes
without a transform step:

```json
{
  "timestamp": "2026-08-08T08:29:54.896902Z",
  "level": "INFO",
  "message": "starting",
  "models": 1,
  "backends": 1,
  "policy": "CacheAffinity",
  "role": "Proxy"
}
```

`--log` (`FASTLLM_LOG`) takes an `EnvFilter` directive, so
`--log 'info,fastllm_proxy::proxy=debug'` turns on per-request routing and
classification detail without the rest.

## Metrics worth knowing about

Most are self-describing from their `# HELP` text. Four are not obvious:

- **`fastllm_classify_escalations_total` is not `fastllm_classified_refined_total`.**
  The second counts prompts the transformer _decided_; the first counts prompts
  it was _asked about_. When it declines, the fast tier's answer stands and is
  counted as `classified_fast`. The gap between them is how often the expensive
  tier ran and changed nothing, and the escalation rate itself is the number the
  two-tier design is justified on.
- **`fastllm_backend_duration_seconds` versus `fastllm_model_duration_seconds`.**
  A model's p99 rising says the model got slow. The per-backend one says which
  replica did, which is the difference between "the provider is degraded" and
  "one of our two GPUs is".
- **`fastllm_upstream_status_total` keeps 429 separate from other 4xx.** It is
  the retryable one, and the reason a pool that passes every health check can
  still refuse a request — lumping it with client errors hides the signal that
  explains a failover.
- **`fastllm_snapshot_age_seconds` compares two machines' clocks**, since the
  stamp is the control plane's. It is clamped at zero rather than going
  negative on skew, which would read as a broken exporter.

- **`fastllm_cache_total{kind="hit"|"miss"|"store"}` counts three things, not
  two.** A miss that is never stored is a response the cache declined to keep —
  streaming, an error, too large — so `store` well below `miss` is the cache
  working as intended on uncacheable traffic, not a bug. `fastllm_cache_entries`
  and `fastllm_cache_bytes` are the live occupancy against the configured
  bounds.

`fastllm_build_info` is a constant 1 carrying the version as a label — the
conventional shape, and what turns "latency moved at 14:02" into "latency moved
when we shipped this".

## Shutdown

On SIGTERM (or Ctrl-C) the proxy stops accepting connections, tells every open
one to stop keep-alive, and waits for in-flight requests to finish before
exiting — `--shutdown-grace` (`FASTLLM_SHUTDOWN_GRACE`, 25s) bounds the wait,
sitting under Kubernetes' 30s `terminationGracePeriodSeconds`.

This matters for a streaming gateway specifically: a generation can run for
minutes, and cutting it produces a response that stops mid-sentence with no
error for the client to retry on. Measured against a 6-second stream with the
signal sent 2 seconds in: at the default the client received every frame
including `[DONE]`; at `--shutdown-grace 0` it received two frames and nothing
else.

If the grace expires with connections still open, they are closed and the
count is logged at WARN — those are requests somebody is still waiting on, and
silence would make the truncation look like a client bug.

## What each replica can see

`GET /admin/fleet` on the control plane reports, per proxy replica, its
backends' health and in-flight counts and the snapshot version it is serving.
Proxies push this every `--health-report-interval` (default 10s) over the same
`--proxy-token` channel as usage.

Two questions it answers that `/metrics` cannot without scraping every pod:
whether the fleet agrees a backend is up — a single replica that disagrees is a
network partition, not a dead backend — and whether every replica is on the
same snapshot version, which is how a pod stuck on an old configuration becomes
visible.

Nothing is stored: a replica that stops reporting ages out after 30 seconds.
"Up, 40 minutes ago" is not health.

## Notifications

Everything this gateway knows is already published — `/metrics` to scrape,
`/admin/fleet` to poll, `usage_events` to query. All of it requires somebody
to be looking. `--webhook-url` is the other direction, for the conditions
worth telling someone about at 3am:

| event                     | when                                                                                                 |
| ------------------------- | ---------------------------------------------------------------------------------------------------- |
| `backend_down`            | a replica newly reports a backend unhealthy                                                          |
| `backend_recovered`       | the same backend reporting healthy again                                                             |
| `snapshot_rebuild_failed` | a rebuild failed _after_ a write committed, so the database and the published snapshot have diverged |

**Transitions, not states.** A backend that is down stays down, and a health
report every ten seconds would otherwise become six alerts a minute for one
incident. A replica's _first_ report emits nothing at all — there is no
previous state to have changed from, and a control-plane restart would
otherwise announce every already-down backend as though it had just failed.

**Per replica, not merged.** Every replica losing a backend is a dead
backend; one replica losing it is a partition. Merging them here would delete
the distinction before anyone saw it, which is the same reason
`GET /admin/fleet` never averages replicas together.

`--webhook-secret` signs each body with HMAC-SHA256 in `x-fastllm-signature`,
in the `sha256=<hex>` form most receivers already know. A webhook endpoint is
reachable by anyone who learns its address, so a receiver that _acts_ on
notifications wants to know where they came from.

Delivery is one attempt with a five-second timeout and no retry, from a small
bounded queue. A receiver that is down will still be down in five seconds,
and a retry loop would turn one unreachable endpoint into a queue that never
drains — while the condition being reported is still true and still visible
on `/metrics` and `/admin/fleet`. Notifications dropped because the queue was
full are counted rather than silently discarded.

## Traces

Built only with `--features otel`, because it is the one part of telemetry that
adds a dependency tree (opentelemetry plus tonic/gRPC) and the only one needing
something deployed to receive it. A build without the feature carries none of
it and pays nothing at runtime — the instrumentation compiles away.

```
--otel-endpoint http://collector:4317   # unset disables tracing
--otel-sample-one-in 100                # 1 traces everything
--otel-service-name fastllm-proxy
```

One span per request, `chat_completion`, carrying the requested model, the
model that actually served it, the backend, whether it streamed, the prompt
class if one matched, the upstream status, and how many attempts it took. That
last pair is the reason to reach for a trace rather than a metric: a histogram
says the 99th percentile moved, a span says _this_ request failed over twice
and landed on the fallback.

Deliberately not recorded as span attributes: the request body, the caller's
principal, or any credential. A tracing backend is a log with a nicer UI, and
prompts do not belong in one.

Two behaviours worth knowing:

- **Sampling is by counting, not randomly.** One request in `n` exactly, rather
  than a ratio that is only right on average — at low volume a random sampler
  means a quiet hour traces nothing. An upstream sampling decision is honoured
  before this one, so the proxy never punches a hole in the middle of somebody
  else's trace.
- **An unreachable collector is not an outage.** If the exporter cannot be
  built at startup the proxy logs it and serves traffic untraced; export itself
  is a background batch task, so a collector that goes away costs dropped spans
  rather than dropped requests.
