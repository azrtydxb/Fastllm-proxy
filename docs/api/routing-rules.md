# Routing rules

The rule grammar behind virtual models, and the dry-run that answers which
rule would decide before anything is dispatched.

A virtual model is a client-facing name with an ordered list of rules and a
fallback. First rule whose conditions match wins; conditions within a rule are
AND'd. Targets are weighted (relative shares, not percentages), and the target
list is a **fallback chain**, not just a split.

| condition | matches on | reads |
|---|---|---|
| `principals`, `roles` | who is calling | request |
| `min/max_prompt_tokens` | estimated prompt size | request |
| `min/max_max_tokens` | requested generation length | request |
| `stream` | whether the client asked for a stream | request |
| `headers` | exact header values, all must match | request |
| `min/max_budget_used_percent` | how much of the caller's budget is spent | snapshot |
| `max_inflight_per_backend` | how busy this rule's own targets are | **live cluster state** |
| `class` | which prompt class the classifier assigned — see [semantic routing](../classifier.md) |
| `after`, `before`, `days`, `utc_offset_minutes` | wall-clock window | **clock** |

The last two rows are marked because they matter: every other condition is a
pure function of the request, so the same request always routes the same way
and prefix affinity means something. A load- or time-dependent rule gives that
up by design — two identical requests a second apart can legitimately land on
different models. Worth choosing knowingly.

Some shapes worth stealing:

```jsonc
// Burst to the cloud only when the local pool is full. First-match-wins does
// the work; there is no separate "spill" mechanism.
[{"position": 0, "max_inflight_per_backend": 2, "targets": ["local"]},
 {"position": 1,                                "targets": ["openrouter"]}]

// Let the client say what kind of work this is.
{"position": 0, "headers": {"x-fastllm-tier": "batch"}, "targets": ["cheap"]}

// Batch work (nobody is watching) goes somewhere slower.
{"position": 0, "stream": false, "targets": ["cheap"]}

// Degrade instead of refusing: past 80% of budget, use the free local model.
{"position": 0, "min_budget_used_percent": 80, "targets": ["local"]}

// Overnight, keep everything in-house. 22:00–06:00 local at UTC+2.
{"position": 0, "after": "22:00", "before": "06:00", "utc_offset_minutes": 120,
 "days": [1,2,3,4,5], "targets": ["local"]}
```

**Failover.** A rule's targets are tried in order. If the first model's whole
pool answers `5xx`, `429`, or cannot be reached, the request moves to the next
model in the same rule — before any byte has reached the client, so nothing is
corrupted. `429` counts because a hosted provider refusing a request is not
the same as being unhealthy: the pool passes every probe and still cannot serve
this call. When the chain is exhausted the last upstream's own status and body
reach the client rather than a synthetic 502.

Failover never widens reach: a candidate the caller lacks `model:invoke` on is
dropped from the chain, so a chain can span models with different grants
safely. Usage is attributed to the model that actually answered.

Malformed conditions (`"after": "25:00"`, `days: [8]`, a percentage above 100)
are rejected by `POST /admin/virtual-models/{id}/rules` with a message naming
the field, rather than stored as a rule that silently never matches.

## Routing dry-run

`POST /admin/routing/dry-run` answers the question a rule author actually has —
"does my `coding` rule fire for this caller?" — without sending a real request
and reading the answer out of a log. It returns the candidate chain and the
index of the rule that decided, because "my second rule matched instead of my
first" and "my first rule matched and points somewhere I did not expect" are
different bugs with the same symptom.

Two honest limits. Backend **health is not consulted**: the registry is built
fresh from the snapshot, so every backend looks up — `GET /admin/fleet` is
where reachability lives. And the prompt **class is supplied, not computed**,
so this tells you what a `coding` prompt would do, not whether some particular
prompt is coding — `POST /admin/prompt-classes/evaluate` answers that one.
