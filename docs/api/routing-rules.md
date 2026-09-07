# Routing rules

The rule grammar behind frontend models, and the dry-run that answers which
rule would decide before anything is dispatched.

A frontend model is a client-facing name with an ordered list of rules and a
fallback. First rule whose conditions match wins; conditions within a rule are
AND'd. Targets are weighted (relative shares, not percentages), and the target
list is a **fallback chain**, not just a split.

| condition                                       | matches on                                                                            | reads                  |
| ----------------------------------------------- | ------------------------------------------------------------------------------------- | ---------------------- |
| `principals`, `roles`                           | who is calling                                                                        | request                |
| `min/max_prompt_tokens`                         | estimated prompt size                                                                 | request                |
| `min/max_max_tokens`                            | requested generation length                                                           | request                |
| `stream`                                        | whether the client asked for a stream                                                 | request                |
| `headers`                                       | exact header values, all must match                                                   | request                |
| `min/max_budget_used_percent`                   | how much of the caller's budget is spent                                              | snapshot               |
| `max_inflight_per_backend`                      | how busy this rule's own targets are                                                  | **live cluster state** |
| `class`                                         | which prompt class the classifier assigned — see [semantic routing](../classifier.md) |
| `min/max_request_cost_micros`                   | what this request would cost, at the cheapest model this frontend model can reach     | **prices** |
| `after`, `before`, `days`, `utc_offset_minutes` | wall-clock window                                                                     | **clock**              |

The last two rows are marked because they matter: every other condition is a
pure function of the request, so the same request always routes the same way
and prefix affinity means something. A load- or time-dependent rule gives that
up by design — two identical requests a second apart can legitimately land on
different models. Worth choosing knowingly.

**What a cost condition is priced against.** There is a circularity to get out
of the way: what a request costs depends on which model serves it, and which
model serves it is what the rule is deciding. Comparing against the rule's own
targets would make the condition answer a different question in every rule, and
a `deny` rule has no targets at all. So the figure is the cost **at the
cheapest model this frontend model could reach** — one number per request,
computed once before any rule is tested, the same for every rule in the chain.
Well defined, order-independent, and monotone: if the cheapest option is over
the cap, every option is.

That settles what it is good for. "Refuse anything that would cost more than
$0.50 however I route it" is exactly this question, which is why it pairs with
`deny`. "Send the expensive ones somewhere cheaper" is not a condition at all —
that is the `cheapest` policy, one level down. A frontend model whose reachable
models are all unpriced has no cost for the request, and a rule naming either
bound does not match: unpriced is unknown, and a guard that fired on unknown
would refuse traffic on the strength of a blank field.

**What `max_inflight_per_backend` counts.** The engine's own number where it
publishes one. Every proxy reads each backend's Prometheus `/metrics` in the
background (`--engine-scrape-interval`, two seconds by default) and routing
compares the ceiling against what vLLM or SGLang says is running plus queued.
That matters as soon as there is more than one proxy: a per-replica counter
means two proxies each admit up to the ceiling, so a limit of 2 spills at
about 4, and traffic that reached the engine without passing through FastLLM
is invisible. Which backends have this is detected, not configured. Each is asked once, then
every five minutes while it produces no reading, and after fifteen minutes it
is assumed not to have metrics and dropped from the scrape — so a deployment of
hosted providers costs a handful of requests per provider for the life of the
process rather than a poll every few minutes for ever. A backend that answers
with something that is *not* engine metrics — a 404, or a Prometheus body with
no scheduler families in it — settles sooner, on the second such answer, since
the endpoint has said as much itself. Editing an endpoint starts the detection
over.

The one verdict that is revisited is the one reached by deadline: nothing ever
replied, so "it has no metrics" was an inference, and an engine that took
longer than fifteen minutes to load a model would be written off by it. Such a
backend gets exactly one more question if it later passes a health probe.

Anything not detected as an engine falls back to this replica's own count, and
so does a backend whose last reading has gone stale, so the condition degrades
to its old behaviour rather than to "idle".

## What a rule does when it matches

Every rule has an **action**, and every action is terminal — the matching rule
decides everything about the request, which is what lets a dry-run answer
"which rule decided this" with one rule name instead of a trace.

| action | what it does |
| --- | --- |
| `route` (default) | send the request to this rule's targets, in the order the policy below produces |
| `deny` | refuse, with `deny_status` (4xx only) and `deny_message` |
| `jump` | continue evaluation in another frontend model's chain, named by `jump_to` |

`route`, `failover`, `balance` and `split` are deliberately **not** four
actions: all four mean "order a chain, try the head, fall down the list on
failure" and differ only in how the head is picked, which is what `policy`
says.

**`deny` is 4xx only**, refused at write time otherwise. A refusal answered
with a 5xx tells every client library in the world to retry a request that will
never be allowed, and reads in an error-rate chart as the gateway failing
rather than as policy working.

**A `jump` that would close a loop is refused when you create it**, by walking
the jumps already stored. The proxy also caps how far it will follow a chain,
because a snapshot can arrive from a hand-edited database — but that is a
backstop, not the mechanism. A jump whose destination has since been *deleted*
is treated as no match, so the request falls through to the next rule; failing
instead would turn deleting one frontend model into an outage for every other
one that referenced it.

Any rule may also carry a **`tag`**, which is copied onto the usage rows that
rule produced. That is a field rather than an action: a rule that tags still
has to route, or there would be no usage row to label. It is what makes spend
answerable per *decision* rather than only per model.

```jsonc
// Refuse rather than route: expressible for the first time.
{"position": 0, "roles": ["batch"], "min_prompt_tokens": 200000,
 "action": "deny", "deny_status": 413,
 "deny_message": "batch keys are capped at 200k-token prompts"}

// Shared policy, written once and referenced.
{"position": 0, "action": "jump", "jump_to": "<house-policy id>"}

// Attribute this rule's spend without changing where it routes.
{"position": 1, "class": "coding", "targets": ["big"], "tag": "team-research"}
```

## Choosing among a rule's targets

By default a rule's targets are a **weighted split**: a deterministic pick on
the request prefix, so a conversation stays on one side of a canary rather than
flipping per request, then the rest of the list in declaration order as the
failover chain.

A rule can name a `policy` instead, and it applies to that rule's own targets:

| policy | picks |
| --- | --- |
| `cache-affinity` | the target holding this prefix's KV cache |
| `least-loaded` | fewest in-flight requests |
| `lowest-latency` | lowest recent mean latency |
| `round-robin` | strict rotation |
| `cheapest` | lowest published price; a target nobody has priced is skipped, not read as free |

Per rule, because that is the level the targets are at — "least-connections
across the two local boxes, then plain failover to the cloud when they are
full" is two rules wanting two different answers, and one setting for the whole
frontend model could not say it. A rule with no policy inherits the frontend
model's, which is also what the *default* targets use since they have no rule
of their own.

**This is not the same knob as a model's own load balancing.** Routing happens
twice: once to choose a model, and once to choose which of that model's
backends serves it. This table is the first. The second is
`provider_models.policy`, set on the Models screen, and it takes the same
values — see [providers](../providers.md).

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
are rejected by `POST /admin/frontend-models/{id}/rules` with a message naming
the field, rather than stored as a rule that silently never matches.

## Routing dry-run

`POST /admin/routing/dry-run` answers the question a rule author actually has —
"does my `coding` rule fire for this caller?" — without sending a real request
and reading the answer out of a log. It returns the candidate chain and the
index of the rule that decided, because "my second rule matched instead of my
first" and "my first rule matched and points somewhere I did not expect" are
different bugs with the same symptom.

A `deny` comes back as a `denied` object carrying the status and message the
caller would receive, with an empty chain — which is a different thing from
"nothing is serving", and the field is what tells them apart. A matching rule's
`tag` comes back too, so you can see what the usage row would be labelled.

Three honest limits, and the third is the one that bites.

Backend **health is not consulted**: the registry is built fresh from the
snapshot, so every backend looks up — `GET /admin/fleet` is where reachability
lives. The prompt **class is supplied, not computed**, so this tells you what a
`coding` prompt would do, not whether some particular prompt is coding —
`POST /admin/prompt-classes/evaluate` answers that one.

And **`max_inflight_per_backend` cannot be evaluated here at all.** The dry-run
runs on the control plane, whose registry has no in-flight counters and no
engine scrape, so every backend looks idle and a spill rule always reports as
still matching. A local-then-spill chain will therefore dry-run as "rule 0,
local" no matter how loaded the hardware is, while real traffic spills
correctly. Only real traffic exercises that condition; `GET /admin/fleet` and
the providers page show the live load the proxies are actually reading.
