# Semantic routing

**Status: shipped**, behind the `classifier` cargo feature and a
`--classifier-model` pointing at the model (the Docker image bakes one in and
sets it). This page records the measurements behind the design, so the
decisions have their evidence attached and nobody re-litigates them from
intuition.

Define a class with example prompts, then name it in a routing rule:

```bash
curl -X POST https://control/admin/prompt-classes -b "$SESSION" \
  -H 'content-type: application/json' \
  -d '{"name":"coding","min_margin":0.05,
       "examples":["Why does this Rust code fail the borrow checker?",
                   "My unit test throws NullPointerException on line 42."]}'
```
```jsonc
{"position": 0, "class": "coding", "targets": ["claude-sonnet"]}
```

`POST /admin/prompt-classes/evaluate` reports, per class, leave-one-out
precision and recall over your own examples, the mean and worst margin, the
nearest other classes, the examples that were misclassified, and a verdict.
Two classes whose centroids sit above ~0.8 are one region with two names and no
threshold separates them — the report says so rather than leaving you to infer
it from four numbers.

Everything here is reproducible:

```bash
python3 bench/fetch-prompts.py          # labelled prompts, cached to bench/data/
cargo run -p bench --release --bin potion        # latency and token-cap sweep
cargo run -p bench --release --bin potion-real   # accuracy on real labelled data
cargo run -p bench --release --bin potion-classes # which classes separate at all
cargo run -p bench --release --bin potion-arch   # architecture vs coding
cargo run -p bench --release --bin minilm        # does a transformer earn its cost
```

Datasets: `HuggingFaceH4/no_robots` (9,499 human-written, human-categorised
prompts), `openai/gsm8k` (1,000 maths word problems), and eleven StackExchange
communities (1,200 each) whose boundaries were drawn by the people asking
rather than by us.

## Why two tiers

| model | p50 per prompt | separates |
|---|---|---|
| potion-base-2M | 8.4 µs | weakest of the static set |
| potion-base-8M | 103 µs | general subject matter |
| **potion-code-16M** | **115 µs** | best on coding (98.7%) |
| potion-retrieval-32M | 137 µs | best all-round on synthetic data |
| all-MiniLM-L6-v2 | 1.66 ms | modest gain over the static tier |
| **bge-small-en-v1.5** | **3.27 ms** | same-subject / different-intent |

Tier 1 is a token-vector lookup and a mean — no transformer, no matmul. Cost
also *plateaus* rather than growing with the prompt, because the encoder stops
at its token cap: a 64 KB paste costs what a 4 KB one does.

Measured token-cap sweep, `potion-code-16M`:

| max_length | p50 | accuracy |
|---|---|---|
| 32 | 32 µs | 76.7% |
| 128 | 115 µs | 80.0% |
| 512 | 460 µs | 80.0% |

128 is the chosen default: on *real* prompts it beat 32 for coding (98.7% vs
98.2%), because a coding question's giveaway is often the pasted code below the
first line rather than the first line itself.

## What tier 1 classifies well

Held out over real labelled prompts at a 0.05 margin floor:

| class | precision | recall |
|---|---|---|
| coding | 97.6% | 92.6% |
| chat | 95.8% | 98.0% |
| generation (creative, long-form) | 96.8% | 69.6% |
| math | 88.0% | 97.6% |
| devops | 86.8% | 90.5% |
| finance | 86.2% | 91.6% |
| legal | 85.9% | 75.0% |
| security | 84.8% | 82.3% |
| factual-qa | 83.7% | 50.4% |
| databases | 82.3% | 91.1% |
| ux-design | 75.6% | 79.8% |
| statistics | 74.0% | 66.5% |

Twelve viable classes on the 115 µs tier. Escalated to tier 2, writing-craft
goes 78.8% → 93.5%, statistics 74.0% → 85.3%, legal 85.9% → 93.2% — tier 2
upgrades good to excellent, but rarely changes whether a class is usable.

## Three findings that shaped the design

**Classify by subject, not by verb.** Subject-matter classes work. Task-shaped
classes — summarise, rewrite, extract, classify — fail on *both* tiers. Under
bge-small, Summarize scores 46.6% precision and Extract 35.6%, *worse* than the
static model's 63.6% and 58.2%. Telling "summarise this" from "extract the
dates from this" needs instruction understanding, not better sentence
embedding, so no embedding tier fixes it. The same shape explains architecture
versus coding: isolated it scores 75.3% (tier 1) and 93.3% (tier 2), but among
eleven domains it collapses to 48.7% and 65.9%, because devops, databases and
data-science compete for the same region.

**Class count is not the problem; class definition is.** In a ten-way run,
Coding scores 83% precision and Chat 90% while Closed QA scores 20% and Extract
34%. Those three describe overlapping ideas. The design therefore does not cap
the number of classes — it makes per-class quality measurable and lets the
confidence floor be per class, since a class at 98% precision and one at 20%
cannot share a threshold.

**Margins are not comparable across models.** bge-small reports an
architecture/code-review centroid similarity of 0.943 against the static
model's 0.621, while classifying the same data considerably better — its
embedding space is anisotropic, packing everything into a narrow cone. A floor
tuned on one tier is meaningless on the other. Floors are per class *and* per
tier.

## The confidence floor is structural

Coding is 3.5% of real traffic, so a classifier at 99% recall can sit at 20%
precision — it over-predicts the rare class, and accuracy hides it completely.
Measured coverage against accuracy, `potion-code-16M`, coding vs everything
else:

| floor | traffic classified | accuracy on it |
|---|---|---|
| 0.00 | 100% | 98.7% |
| 0.05 | 96% | 99.4% |
| 0.10 | 88% | 99.9% |

Below the floor a rule simply does not match and the next rule catches it —
first-match-wins semantics, not a special case, not an error.

## How the tiers are gated

`Classifier::escalate_from` is the set of tier-1 class names that some
**active** tier-2 class refines, computed at snapshot build. If no routing rule
references a tier-2 class the set is empty, the transformer is never loaded,
and no request can pay for it. A deployment using only tier-1 classes is
indistinguishable at runtime from one built before tier 2 existed.

When a request does escalate, tier 2 decides only between the classes that
named that tier-1 class — a narrower question than the full taxonomy, and
measurably an easier one.

On realistic traffic mixes escalation touches well under a tenth of requests,
putting the average added cost near 0.2 ms.

## Classes compete globally

Every class in the snapshot is scored on every classified request. Two classes
seeded with neighbouring prompts produce neighbouring centroids, and the margin
between them collapses below any floor — so both stop matching and requests fall
through. `POST /admin/prompt-classes/evaluate` reports exactly this as a
collision, and it is the first thing to check when a class that looks correct
stops firing.

Per-request classification is logged at debug with the class, the margin and
which tier decided, so drift is visible before somebody complains about answers.

## Memory

Both models ship in the image. Loaded, they cost real memory in whichever
process uses them — measured on the dev cluster at 272-275 Mi for a control
plane with both tiers, 268 Mi for a proxy that has lazily loaded the refined
tier, against 85 Mi for a proxy doing no classification. Size limits
accordingly; [deploy/README.md](../deploy/README.md) has the table.

## Not GPU work

Tier 1 is a memory lookup, not a matmul: **94,450 prompts/s on a single core**,
measured. PCIe transfer alone would exceed the whole compute budget. Tier 2 is
a real transformer and would batch well on a GPU, but this workload is
single-request and latency-critical — there is nothing to batch — and the GPUs
in this deployment are busy serving the model. Both tiers stay on CPU.

## Refined classes come in pairs

A refined class only takes effect when at least **two** of them refine the same
fast-tier class. That is not a limitation, it is the shape of the question: the
measurement behind this feature is binary — architecture *against* coding, at
93.3% — and a lone refined class has nothing to be compared against.

With one contender there is no runner-up, so the margin degenerates to a raw
similarity score, and a margin-shaped floor like 0.10 is met by almost any
prompt's similarity to almost any centroid. That one class would then capture
every request the fast tier assigned to the class it refines. Escalation with
fewer than two contenders is therefore skipped and the fast tier's answer
stands.

So to split coding into architecture and debugging, define *both* as refined
classes, both refining `coding`.

A refined answer still satisfies a rule naming the class it refines. `debugging`
is a kind of `coding`, so an existing `{"class": "coding"}` rule keeps matching
after you add the refinement — put the more specific rule *earlier* in the chain
to separate them. Without that, defining a refined class would silently stop
every rule on its parent from firing, which is a change nobody asked for and
nobody would see.

## What is not built

One thing deliberately out of scope: routing on *difficulty*. GSM8K separates
from factual lookup at 96%, but GSM8K has a very distinctive narrative-maths
genre, so that number most likely measures genre rather than difficulty. It
would need an experiment against hard prompts that read like easy ones before
it became a feature.

---

Back to the [README](../README.md).
