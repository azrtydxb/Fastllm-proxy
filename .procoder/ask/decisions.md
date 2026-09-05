
## Keep or delete the unusable RadixArk NVFP4 checkpoint on both Sparks

`RadixArk/Qwen3.8-Flash-Next-NVFP4` (126 GiB per node, 252 GiB total) cannot be
loaded by the `qwen38-flash-next` vLLM build: it quantizes the n-gram embedding,
and `Qwen3_8FlashNextNGramEmbedding` has no `weight_scale` parameter to receive
it. It has been superseded by `Inferact/Qwen3.8-Flash-Next-NVFP4`, now
downloading. Disk is not under pressure (700 GB / 659 GB free).

- **Keep it** — costs nothing today; still there if a later vLLM build gains
  support for a quantized n-gram embedding, or for comparing quant quality.
- **Delete it** — reclaims 126 GiB per node now; re-downloading is ~1 hour if
  ever wanted again.

## Which checkpoint + serving path for Qwen3.8-Flash-Next on the Spark pair

My earlier claim that RadixArk "cannot load" was too strong. Correct statement:
RadixArk's PLE/n-gram table is FP8-quantized (`model-plefp8-*.safetensors`), and
**stock** vLLM's `Qwen3_8FlashNextNGramEmbedding` is a plain
`VocabParallelEmbedding` with no `weight_scale` — hence our load failure at
`ple_layer.py:530`. Community patches replace that loader and it works.

Evidence found:
- `blazux/qwen3.8-Flash-DGX` — RadixArk + vLLM + the same vendor image, on a
  SINGLE GB10 Spark. Patches the exact file in our traceback
  (`vllm/models/qwen3_8_flash_next/nvidia/ple_layer.py`) to mmap the 44 GiB PLE
  table from NVMe. Weights drop to ~76 GiB. Measured ~2,400-2,660 tok/s prefill,
  ~27 tok/s decode with MTP=2.
- `MiaAI-Lab/Qwen3.8-Flash-Next-Dual-DGX-Sparks` — RadixArk on TWO Sparks, TP2,
  but SGLang, with its own SM121 QSA kernel patches. Measured 64 tok/s
  single-stream, 117 tok/s at 2x concurrency.
- Inferact NVFP4 leaves PLE unquantized (170 GiB, ~85 GB/node at TP2), so stock
  vLLM should load it with no patch — but nobody found running it on GB10.

Options:
- **Inferact + stock vLLM TP2** — no patches; download already ~37/170 GiB in.
  Unproven on GB10.
- **RadixArk + blazux mmap patch** — proven on GB10 with vLLM; reuses the 126 GiB
  already on disk; needs a patched image build.
- **RadixArk + SGLang TP2 (MiaAI-Lab)** — proven on this exact 2-Spark hardware;
  abandons the vLLM/sparkrun path.

Independent of the choice: `--no-enable-prefix-caching` is REQUIRED on GB10 (GDN
kernel bug corrupts the cached-block path) and torch.compile must be off
(Inductor int64 assert on sm_121). My recipe currently enables prefix caching.

## How to improve Qwen3.8-Flash-Next decode throughput on the Spark pair

Baseline measured: 26 tok/s single-stream, 69 tok/s aggregate at 4 concurrent
(2.7x scaling, so decode is not compute-saturated).

Key measurement: during generation the head node reads ~42 MB/s from NVMe, and
has only 12 GB of page cache against a 44 GiB PLE/n-gram table. The mmap patch
gathers n-gram rows from disk every decode step, and `gpu_memory_utilization:
0.80` has taken ~97 GB of the shared 121 GB pool, starving the page cache that
would otherwise hold the hot rows. On a unified-memory box, GPU budget and page
cache are the same RAM.

Options:
- **Lower gpu_memory_utilization to ~0.55-0.60, plus VLLM_PLE_MMAP_PREWARM=1** —
  frees ~40 GB for page cache, potentially caching most of the hot table. Also
  fixes the 10 GB headroom risk (blazux warns an OOM can freeze a Spark). Free,
  one restart. KV is cheap for this arch so 0.55 should still be ample.
- **MTP 2 -> 3, and/or --async-scheduling** — small config wins, one restart each.
- **Two TP1 replicas instead of one TP2** — TP2 measured 26 tok/s vs blazux's
  ~27 single-node, so cross-node TP buys no decode speed, only KV headroom.
  Two independent replicas would roughly double aggregate throughput.
- **Switch to SGLang (MiaAI-Lab recipe)** — measured 64 tok/s single-stream and
  117 at 2x concurrency on exactly this 2-Spark hardware, via SM121 QSA kernels
  + NVFP4 KV cache + NEXTN 3/1/4. ~2.5x, but abandons vLLM/sparkrun.
- **Remove the per-step host<->device sync in the mmap gather** — blazux names
  this as the known decode cost and invites a pinned-buffer PR. Real dev work.

## Throughput: config tuning is exhausted, pick a structural direction (and a context size)

Five configurations measured (300 tok, concurrency 1 and 4):

| Run | Config                          | single | agg@4 |
|-----|---------------------------------|--------|-------|
| A   | gpu_mem 0.80, MTP 2             | 26.0   | 69.2  |
| B   | gpu_mem 0.55 + prewarm, MTP 2   | 24.7   | 73.4  |
| C   | B + MTP 3                       | 25.5   | 65.5  |
| D   | B + async-scheduling            | 23.5   | 68.0  |
| E   | B + expert-parallel (x2 runs)   | 24.4 / 24.2 | 70.3 / 74.7 |

E repeated twice gives aggregate 70.3 vs 74.7 — so the aggregate metric carries
~6% run-to-run noise, and every aggregate comparison above except MTP 3's loss is
inside it. Single-stream is stable (+/-0.2). Conclusion: no config flag helps;
best single-stream remains baseline A at 26.0.

Remaining levers are structural, since the cost is ~1,615 RoCE packets/token of
per-layer all-reduce (PYNCCL is the only backend this hardware offers) plus the
mmap gather's per-step host<->device sync.

- **Two TP1 replicas** — model fits on one Spark with the mmap patch. Zero
  inter-node traffic, ~2x aggregate, no single-stream gain.
- **SGLang (MiaAI-Lab)** — 64 tok/s single-stream / 117 at 2x concurrency
  measured on this exact 2-Spark hardware. ~2.5x, but a different stack.
- **Pinned-buffer patch** to remove the per-step sync blazux names. Real dev work.
- **Accept current** and spend the effort elsewhere.

Separately: KV is barely used (19.91 GiB of an available 62.95 GiB), so the
65536 context is our cap, not the model's (native 262144).

## MiaAI-Lab's 64.4 tok/s does not reproduce on our pair — what now

Their stack was reproduced faithfully: same hardware (2x GB10), same checkpoint
(RadixArk NVFP4), same patched image, NCCL 2.30.7 LD_PRELOADed, NCCL RoCE
pinning identical, NEXTN 3/1/4, NVFP4 KV, PLE offload on, CPU pinning 5-9,15-19,
mem-fraction 0.80, 262144 native context.

Run under THEIR harness (sglang bench_serving, concurrency 1):
  ours:   25.55 tok/s output, TTFT 313 ms, TPOT 38.4 ms
  theirs: 64.4  tok/s output, TTFT 117 ms

Decomposition: tok/s = accept_length x steps/s. Our steps/s is 11-14 and accept
is content-dependent (1.65 prose+thinking, 2.5 prose, 3.0 predictable list; max
possible 4). Even perfect acceptance at our step rate caps at ~48-56 tok/s, so
their number needs both near-perfect accept AND a ~16/s step rate.

Corroborating context: our vLLM TP2 measured 25-26 tok/s, and blazux measured
~27 tok/s on a SINGLE Spark with vLLM+MTP2. Three independent configurations
land at 25-27. MiaAI-Lab's 64.4 is the outlier.

Remaining undocumented deltas: their perf table may have been measured on their
1M YaRN .env (CONTEXT_LENGTH=1048576, MEM_FRACTION_STATIC=0.82,
MAMBA_FULL_MEMORY_RATIO=0.3) rather than the script defaults we ran.

Options:
- Accept ~25-30 tok/s as this model's real speed on two Sparks; pick a stack and
  port it to a sparkrun recipe.
- Try their exact 1M YaRN .env, the one remaining documented difference.
- Open an issue on their repo with our reproduction data and ask for their
  measurement conditions.

## DFlash2 27B on one Spark: tuning results and whether to continue

Built vLLM v0.27.1 + DFlash2 overlay natively for arm64/sm_121 (~35 min compile,
first known GB10 build of this stack). Coding benchmark, 192-token outputs:

| Config                         | c1   | c4    | c8    | accept |
|--------------------------------|------|-------|-------|--------|
| Stock 5090 profile             | 31.6 | 136.0 | 95.6  | 3.0-3.5|
| Adapted (seqs16/32768/32GiB KV/capture->128) | ~37 | ~110 | ~160 | 3.55-3.60 |
| + dynamic-K [[1,4,7],[5,16,3]] | ~29  | ~58   | ~123  | 2.1-2.9 |

Adapting to the proven Spark values fixed the c8 collapse (+51%) and improved c1
(+18%); the c4 column is too noisy (+/-16%) to read. Dynamic-K lost on every axis
and was reverted -- likely because graph capture must cover two K values, halving
coverage per K on a box where graph memory was never the constraint.

Best so far is fixed K=7 with the adapted capacity settings. For reference the
Flash-Next MoE across BOTH Sparks measured 58.7 / 168.2 / 241.2.

Three upstream bugs found and worked around (worth one issue):
1. `[N/A]` VRAM preflight is a bash syntax error on any unified-memory GPU (GB10).
2. DYNAMIC_SCHEDULE documented form loses its quotes to `set -a; source .env`.
3. Dynamic-K documented as {batch:K} dict; vLLM wants list[(start,end,K)].

Remaining untested levers: MAX_NUM_SEQS=32 (needs capture sizes to 256, longer
boot), a fixed-K sweep (K=5 vs 7 -- his K was tuned on a 5090 accepting 55-64%
where we accept ~51%), and --async-scheduling (in your old config, not exposed
by his compose).

## Port the SGLang MoE stack to a sparkrun recipe?

The MoE currently serving on both Sparks runs under PixelML's own
`start-cluster.sh`, not sparkrun -- `sparkrun cluster status` reports no
containers. It is also the best-performing configuration measured today
(c8 ~244 tok/s), so it is the one most worth having as a recipe.

Recipes that DO exist in sparkrun:
- qwen38-flash-next-nvfp4.yaml  -- MoE on vLLM + mmap patch, TP2 (~25 tok/s)
- qwen38-27b-dflash2-k7.yaml    -- 27B DFlash2, single node (41 tok/s c1)
- qwen38-27b-dflash2-k7-tp2.yaml -- 27B DFlash2 TP2, fails in headless worker

Porting cost is lower than the DFlash2 port was: sparkrun has a first-class
sglang runtime with 2-node TP2 recipes, and the reusable fixes are already
known (executor_config entrypoint "", `cd` before serve, HOME/cache redirect).
The stack-specific parts are PixelML's XQA-patched qwen_sparse_attn_backend.py
(bind-mounted into the container) and their API-key launcher, which can be
dropped.

Options:
- Port it now, leaving the current server running, and only cut over once the
  sparkrun version benchmarks equivalently.
- Leave it on PixelML's scripts -- it works, and the scripts are pinned and
  reproducible on their own.
- Port it later; capture today's findings first (the session has produced a
  lot of unrecorded results and six upstream issues).

## The `!` corruption: option 2 turns out to be a no-op, need the source endpoint

Investigated MiaAI-Lab PR #6 ("Fix online-softmax kernel to avoid infinite
thinking `!`", unmerged). Its own diff comment says the cause was THEIR bespoke
Triton online-softmax kernel: "diverged on real NEXTN verification traffic:
tool-enabled xhigh prompts collapsed into a repeated punctuation token while
this kernel and the stock shim did not."

The PR's fix is to stop using the bespoke kernel and instead backport
sgl-project/sglang#36556 -- enabling the existing FlashInfer TRTLLM sparse-decode
path on SM12x, which is *exactly what PixelML's XQA patch already does*
(`if not (is_sm100_supported() or is_sm120_supported())`).

So the stack we are running does NOT contain the buggy kernel; MiaAI's fix
converges ON PixelML's approach. Porting it is a no-op.

Could not reproduce `!` on the current endpoint across: short reasoning, long
reasoning (19k chars), multi-turn, tools+default, tools+xhigh. Zero punctuation
runs in any.

DID reproduce a real defect: runaway reasoning. Long prompts hit finish=length
with 4.5k-19k chars of reasoning and ZERO content -- the model never exits
thinking. Same failure family as their issue #5 "doom loops".

Options:
- The `!` was seen on the EARLIER server (MiaAI stack, :8888 with auth) which
  does contain the buggy kernel -- then the current stack is already the fix.
- It was seen on the current sparkrun endpoint (:8000, no auth) -- then we need
  the exact request that triggered it, since five prompt shapes did not.
- Chase the runaway-reasoning defect instead, which is reproducible now.

## Unifying the reasoning field name across the two Sparks

The two endpoints disagree on the response schema and neither can be configured
to match the other:

  - Qwen3.8-27B on .246 (SGLang) emits `reasoning_content`
  - Qwen3.6-35B-A3B on .245 (vLLM) emits `reasoning`

vLLM's own source calls `reasoning_content` deprecated and renames it to
`reasoning` "so downstream code only needs to check one field". It accepts the
old name on input but always emits the new one. SGLang's `--reasoning-parser`
only selects how thinking is split, not what the field is called.

Consequence: a client hardcoded to one name shows blank reasoning against the
other server -- the exact trap that cost several turns of misdiagnosis today.

Options:
- **Move the MoE to SGLang** so both emit `reasoning_content`. We have the image
  on both nodes and a validated recipe pattern; the MoE would also gain NEXTN
  speculative decoding. Cost: re-validating a container that has worked for
  weeks, and today's migrations each surfaced real surprises (chat-template
  runaway, parser vocabulary mismatch, six fixes on the DFlash2 port).
- **Move the 27B to vLLM** so both emit `reasoning`. Discards the SGLang setup
  tuned and measured today.
- **Normalise in Fastllm-proxy.** Mapping `reasoning` <-> `reasoning_content`
  across heterogeneous backends is a genuine gateway feature and arguably belongs
  there rather than in either server. Does not help until the proxy is deployed.
- **Leave it and handle client-side.** pi is pointed at the 27B only, so nothing
  is broken today; the divergence bites when a client talks to both.

## How much of the kw cluster to bring back up

kw was shut down deliberately, not broken: all 8 nodes cordoned and 34
deployments/statefulsets scaled to 0 across ~15 namespaces (monitoring,
novaflow, novamail, novachess, kryton, registry, operators, novamem-bench).

Already done, and safe: uncordoned all nodes; the Longhorn driver-deployer
CrashLoopBackOff and the 16-day-dead ARC runner listeners both turned out to be
symptoms of the cordon and recovered on their own once scheduling worked; trivy
moved from Pending to Init; the fastllm manifests were re-applied (the existing
Deployments had an immutable-selector mismatch, so they were deleted and
recreated from deploy/).

fastllm now sits in ImagePullBackOff on
192.168.10.123/azrtydxb/fastllm-proxy/proxy:0.2.0 because its image registry
(zot, in the registry namespace) is itself scaled to 0. Postgres data is intact
(PVC Bound, 5Gi on Longhorn) and CNPG reports the cluster healthy.

Options:
- **Minimum path to fastllm**: scale up registry/zot + registry/redis and
  fastllm-system/fastllm-operator only. Three scale-ups, leaves the rest of the
  shutdown untouched.
- **Restore the whole cluster**: scale all 34 workloads back to their previous
  replica counts. Brings back several databases that were deliberately stopped.
- **Stop here**: leave the scale-ups to the user, since the reason for the
  shutdown (cost, maintenance, hardware, mid-migration) is not visible from the
  cluster and some of these are stateful.

## How to start building the FastLLM Claude skills

Design settled: 12 API skills covering all 76 openapi.json paths (one per resource
domain, since 76 one-per-endpoint skills would collide on description matching),
plus fastllm-operate and fastllm-backends for the operational failures no endpoint
reference would have prevented.

Tables and endpoint references generate from openapi.json so they cannot drift;
traps are hand-written because no spec contains them (position NOT NULL on
virtual_model_defaults, virtual names shadowing concrete ones, relative weights,
SQL writes bypassing the audit trail).

Options for the first increment:
- **Generator + one worked skill (fastllm-routing)** so the shape can be judged
  before twelve exist. Routing is the one I got wrong today, so it exercises the
  traps section properly.
- **Descriptions first, all 14**, tested against real phrasings for disjoint
  triggering, before any content is written. Triggering is the part that decides
  whether a skill is ever used.
- **Build all 14 in one pass** and review as a set.
- **Start with fastllm-operate instead**, the highest-value non-API skill, since
  it covers the most expensive failures of this session.

## Naming the BGE virtual models

Rule given: name virtual models after the concrete model, dropping the nvfp4
suffix. That works for the two Qwen models and is now live:

  qwen3-8-27b      -> qwen3-8-27b-nvfp4
  qwen3-6-35b-a3b  -> qwen3-6-35b-a3b-nvfp4

It cannot work for the other two. `bge-m3` and `bge-reranker-v2-m3` carry no
nvfp4 suffix, so "the model name minus nvfp" IS the concrete name, and the API
refuses with 409: a model and a virtual model cannot share a name, since a
client request naming it would be ambiguous. Verified against the live API.

Options for those two:
- Keep the purpose names `embed` and `rerank` (what they had before).
- Suffix them, e.g. `bge-m3-vm` / `bge-reranker-v2-m3-vm`.
- Rename the concrete models to carry a distinguishing suffix, freeing the plain
  names for the virtual ones. Touches every client already calling them.
- Leave them without virtual models; clients address the concrete names, which
  route to themselves.

## Backend-registration agent: what to do with the design

Design is settled in prose (static/dynamic tiering via `model_backends.managed_by`,
lease-based expiry for dynamic only, advisory identity audit for static). Open:

- File it as a GitHub issue now, framed on the static/dynamic tier split.
- Write it to `docs/` in the repo instead of an issue.
- Keep refining the design in conversation before committing it anywhere.

Sub-decisions still unanswered from the first pass:

- `on_unknown_model` default: `report` (agent flags a served model with no matching
  FastLLM model) vs `create` (agent auto-creates the model row).
- Node registry surface: its own `/admin/nodes` vs folding into `/admin/fleet`
  (they are different fleets — proxy replicas vs model hosts).

## Provider as a first-class record: three open policies

Design settled: `providers` table owns api_base + credential + protocol/auth
(moved off `model_backends`, which becomes model_id/provider_id/upstream_model).
Three kinds: cloud (from catalogue), static (typed), dynamic (registrar lease).
Only dynamic enforces health expiry. Open:

- Learned dynamic model whose name matches an existing static/cloud model:
  attach as an extra backend (free replicas/spillover) vs keep separate and flag.
- Learned name collides with a frontend/virtual model name (409, and likely in
  practice given frontends are named after models): conflicted state vs
  auto-suffix vs reject the provider's model.
- Scope: one issue, or split into provider table + cloud catalogue / registrar
  service / frontend resilience.

## "Backend models" -> "Provider models": how deep does the rename go

9 mentions, all prose (web/src 4, src 2 doc comments, docs 3). No identifiers.
Lands inside change (A), which already breaks the snapshot wire format.

- UI + docs only; `/admin/models` and the `models` table keep their names.
- Also rename the API route to `/admin/provider-models`, riding A's existing
  breaking change rather than paying for a second one later.
- Rename the DB table too (`models` -> `provider_models`).

## RBAC on frontend models: the per-target filtering is gone

Implemented and in CI. Authorisation checks the name the caller used; a grant
on a frontend model authorises the whole chain it routes to. The failover
chain used to be filtered per target.

Consequence: adding a target to a frontend model extends the reach of everyone
holding it, with no new grant on record.

- Accept it. It follows necessarily from "RBAC on frontend models" — requiring
  the provider-model grant as well would mean renaming one still revokes
  access, which is the problem #8 exists to fix. Mitigated by editing a
  frontend model needing `config:write`, which already grants everything.
- Keep per-target filtering in addition to the frontend-model grant, and accept
  that provider-model names stay load-bearing — which means the registrar
  cannot churn them and #13 needs a different design.

## Should the five Spark endpoints become dynamic providers?

The node agent now runs on both DGX Sparks and registers all five endpoints,
but every one comes back `kind=static leased=False`: they were configured by
hand first, and registration deliberately never converts a static provider into
one that can expire. So the lease, the health-based degradation and the model
reconciliation currently do nothing for them — they are probed advisorily and
that is all.

The original ask was "we swap models regularly, register the backends and
delete them when they stop running", and that needs `kind = dynamic`.

Consequence of converting: a dynamic provider that stays degraded for 30
minutes is deleted along with its provider models, and `reconcile_models` will
add and remove provider models on every sweep to match what the host serves.
Usage survives (migration 0031) and frontend model targets bind by name (0036),
so a frontend model keeps its target as a dangling name rather than being
destroyed — but the provider models themselves go.

- Convert all five now, in a migration rather than a hand-written UPDATE.
- Convert one host first (dgx-spark2, 2 endpoints), watch a real model swap
  reconcile, then do the rest.
- Leave them static. The agents keep registering *new* endpoints as dynamic,
  and the five hand-made ones stay under human control.

## Stable identity: fix the name-bound links, change the key type, or both?

Asked for: "internally use uuid for providers, models, frontend models, users
and so on... I don't want renames to break links. What we see is just a display
name we can edit."

What the survey found: **the key type is not what breaks on rename.** All 17
tables already have a `BIGSERIAL id` with 19 real foreign keys between them, so
every id-based link already survives a rename. Renames break things because a
few links deliberately bind by *name*, and because almost nothing can be
renamed at all — only `providers` has a `name` on its PATCH route.

The name-bound links, and whether they should stay that way:

- `permissions.resource = 'model/<name>'` — a grant stops matching when the
  model is renamed. Fails closed (403), and is the reason `PatchModel` has
  never had a `name` field. Should become id-bound.
- `rule_targets.target_model_name`, `frontend_model_defaults.target_model_name`
  — routing. Deliberately name-bound by migration 0036 so a deleted and
  re-registered model reattaches by itself. Wants id *and* name: id while the
  row exists, name to reattach after it does not.
- `target_provider_name` — descriptive only; routing never reads it.
- `usage_events.model_name` / `provider_name`, `usage_rollup_hourly.model_name`
  — deliberate (0031). History must record what a thing was called *at the
  time*; these should stay names.
- `frontend_models.name` — the name on the wire. Renaming it is *meant* to
  change what clients ask for; that is the feature, not a broken link.

Options:

- A only: make grants id-bound, give targets an id binding with the name kept
  as the reattachment fallback, and add `name` to the PATCH routes for provider
  models, frontend models, principals, roles, MCP servers and agents. Delivers
  "renames don't break links" in full. No key-type change.
- A then B: A, then migrate all 17 primary keys and 19 foreign keys from
  `BIGSERIAL` to UUID, touching 23 admin routes and ~117 i64 id sites, the
  snapshot, the UI and every test. Adds non-enumerable ids now that ids are a
  stable public contract; adds nothing to rename-safety beyond A.
- B only: key type now, name-bound links later. Rejected in the writing — it is
  the expensive half with none of the benefit the request asked for.
