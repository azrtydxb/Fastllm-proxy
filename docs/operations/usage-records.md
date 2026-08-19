# Usage records and prices

The per-request row behind every spend figure, how long it is kept, and where
prices come from.

## Per-request records

`usage_events` carries latency and outcome alongside the token counts:
`duration_ms`, `ttft_ms`, `status`, and `requested_model` when the client asked
for a name that differs from the one that served it — a virtual model, or the
head of a chain that failed over.

**One row per request that reached a backend**, whether or not the response
carried token counts. `usage_reported` says which: `false` means the counts are
unknown, not zero, and such a row has `cost_micros` NULL rather than 0. Any
query that sums tokens or spend should filter on it — `WHERE usage_reported` —
while any query counting _requests_ must not, since the rows it would exclude
are disproportionately the ones that failed.

**Refusals are recorded too**, and `refusal` says which kind. It is NULL for
every row describing a response a backend actually returned, and set for the
four cases the gateway decides itself:

| `refusal`       | status | meaning                                  |
| --------------- | ------ | ---------------------------------------- |
| `authorisation` | 403    | authenticated, but not granted the model |
| `rate_limit`    | 429    | over a configured per-minute limit       |
| `budget`        | 402    | budget window exhausted                  |
| `no_backend`    | 502    | nothing in the chain could be reached    |

`no_backend` is why the column exists. A refused request has no forwarded
response body, and the body is what writes the row — so before this, a total
backend outage produced _no rows at all_ and an error rate computed here read
a flat zero at exactly the moment nothing worked.

Keep the two apart when charting. `refusal IS NULL AND status >= 400` is
"errors an upstream returned"; `refusal IS NOT NULL` is "requests the gateway
turned away". Blending them into one error rate tells an operator to do
neither of the two available things — raise a budget, or go and look at a GPU
node.

### Retention

`usage_events` takes one row per request now, so it has a policy rather than
needing watching: **raw rows for 90 days, hourly rollups beyond that, kept
indefinitely.** An hourly task folds everything past the cutoff into
`usage_rollup_hourly` and deletes the rows it summarised — in one
transaction, because doing the two separately would lose any request written
between the summary and the delete.

`/admin/timeseries` reads both tables and unions them, so a chart does not
end at the retention boundary. What changes across it is granularity, and one
thing more:

**Rolled-up buckets report no latency at all.** Percentiles do not merge —
averaging two hours' p95 produces a number that is the p95 of nothing — so
the rollup stores `duration_ms_sum` and `duration_ms_count` and the API
returns `null` for p50/p95 over rolled-up data. The chart breaks its line
there, the same as for an empty bucket, rather than drawing a continuous
line whose meaning silently changed 90 days back. A mean is recoverable from
the two stored columns by anyone who wants one.

To keep raw rows longer, change `RAW_RETENTION_DAYS` in `src/control/api.rs`;
the roll-up is additive (`ON CONFLICT DO UPDATE`), so a longer window simply
folds later.

**Refusals with nobody to attribute them** — a 401 from an invalid key, a 404
for a model that does not exist — are in `gateway_rejections` instead, and
`/admin/timeseries` reports them as `refused_unattributed`. So a
caller-visible error total _is_ answerable from Postgres alone; it is simply
two tables, because the two kinds of failure are shaped differently.

They are counts bucketed to the minute per replica, not rows per request, and
that is deliberate: 401 is the one refusal an anonymous stranger can trigger
at will, so a row apiece would let unauthenticated traffic drive unbounded
writes. The counters ride the health report the proxies already send every
ten seconds; the control plane holds each replica's previous report, so it
stores the delta. A counter that went _down_ means that replica restarted,
and the new value is taken as the delta rather than producing a negative. A
replica's first report is skipped rather than counted from zero — its counter
covers however long that process has been alive, and charging all of it to
the current minute would draw a spike that never happened.

These rows carry no model and no principal, so they are **excluded whenever
you filter by either**. A filtered view that included them would attribute
anonymous failures to whichever model or caller you happened to be looking
at.

This is where per-caller detail lives, and deliberately not in Prometheus: the
answer to "which callers got slow" is per principal and per key, and a label
with that cardinality is how a metrics endpoint becomes an outage. Here it is a
column in a database, already batched off the request path.

```sql
SELECT p.name, count(*), percentile_cont(0.95)
         WITHIN GROUP (ORDER BY u.ttft_ms) AS p95_ttft_ms
FROM usage_events u JOIN principals p ON p.id = u.principal_id
WHERE u.at > now() - interval '1 hour' AND u.ttft_ms IS NOT NULL
GROUP BY p.name ORDER BY p95_ttft_ms DESC;
```

Every new column is nullable, and that is load bearing. `ttft_ms` is NULL for a
non-streaming response, where it would be a copy of `duration_ms` rather than a
second measurement; all four are NULL on a row written by a proxy that predates
them. A zero would be indistinguishable from a request that answered instantly.

One limit worth knowing: a usage row exists only for principals whose
consumption is tracked — those with a budget or a token rate limit — because
nothing else parses the response body. Per-model _metrics_ cover every request;
per-caller _rows_ cover those.

## Keeping prices current

`fastllm-proxy sync-prices --database-url "$URL"` fills in any model whose
price is unset, from OpenRouter's published list and the community catalogue.
`--dry-run` first; the next snapshot rebuild picks the change up, with no
restart.

Worth running on a schedule, and worth knowing what it is _not_: where a
provider reports what it actually charged — OpenRouter returns `usage.cost`
unasked — that figure is used instead and this table is never consulted. The
sync matters for providers that publish a price but do not report one per
request.
