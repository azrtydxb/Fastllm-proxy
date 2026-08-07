# Classifier routing

**Goal.** Let an operator route on what a request *is about*, with as many
classes as they want, and know which of those classes are actually trustworthy.

**Not in scope.** Difficulty routing ("does this need the expensive model").
The GSM8K-vs-lookup result is 96% but its construct validity is unproven — it
most likely detects the maths-word-problem *genre* rather than difficulty. That
gets its own experiment before it gets a feature.

## What the measurements settled

Run `python3 bench/fetch-prompts.py && cargo run -p bench --release --bin potion-real`.
Against 9,499 human-labelled prompts (`HuggingFaceH4/no_robots`):

- **Cost is not a constraint.** 8–10µs for a short prompt, and cost plateaus
  with prompt size because the encoder caps at `max_length` tokens. At cap 128
  a worst-case prompt is ~140µs — 0.17% of the 83ms TTFT this proxy currently
  adds nothing to. Pure CPU, so the no-I/O invariant is untouched, and
  deterministic, so unlike `max_inflight_per_backend` it costs no prefix
  affinity.
- **`potion-code-16M` is the best model here**, not the worst. 98.7% accuracy
  on coding-vs-other at cap 128, versus 93.2% for `potion-base-8M`. The earlier
  synthetic benchmark said the opposite because its hand-written prompts had no
  pasted code in them.
- **Class count is not the problem; class *definition* is.** In the ten-way run
  Coding scores 83% precision and Chat 90%, while Closed QA scores 20% and
  Extract 34%. Those three weak categories describe overlapping ideas. Capping
  the number of classes would have been solving the wrong variable.
- **Class imbalance makes accuracy a liar.** Coding is 3.5% of real traffic, so
  a classifier at 99% recall can sit at 20% precision. Per-class numbers are the
  only honest ones.

## The design

### Classes are operator-defined, and unbounded in number

A **class** is a name plus example prompts. That is the entire configuration —
no training pipeline, no labelled corpus, no model to fine-tune.

```sql
CREATE TABLE prompt_classes (
    id           BIGSERIAL PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    description  TEXT NOT NULL DEFAULT '',
    -- Per class, because a class at 98% precision and one at 20% must not
    -- share a threshold. NULL means "use the deployment default".
    min_margin   REAL NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE prompt_class_examples (
    id        BIGSERIAL PRIMARY KEY,
    class_id  BIGINT NOT NULL REFERENCES prompt_classes(id) ON DELETE CASCADE,
    prompt    TEXT NOT NULL
);
```

Routing then uses one new rule condition:

```jsonc
{"position": 0, "class": "coding", "targets": ["claude-sonnet"]}
{"position": 1, "class": "summarise", "targets": ["cheap-local"]}
{"position": 2, "targets": ["default-model"]}   // unclassified falls through
```

### The control plane trains; the data plane only compares

The split already in place does the work:

- **Control plane**, at snapshot build: embed every example, average per class,
  normalise. Ship the centroids in the snapshot. This is the only place the
  model is loaded for *fitting*.
- **Data plane**, per request: embed the prompt, dot-product against N
  centroids, take the best. N is small and the vectors are 256-dimensional, so
  this is a few microseconds on top of the embedding.

Adding a class or an example is a snapshot rebuild, exactly like adding a
backend. No retraining step exists to fail.

### The confidence floor is per class, and load-bearing

A rule with a `class` condition matches only when the winning class is that
class **and** its margin over the runner-up clears that class's `min_margin`.
Below the floor the rule simply does not match, and the next rule catches it —
the ordinary first-match-wins semantics, not a special case.

Measured coverage/accuracy for `potion-code-16M` on coding-vs-other, cap 128:

| floor | traffic classified | accuracy on it |
|---|---|---|
| 0.00 | 100% | 98.7% |
| 0.05 | 96% | 99.4% |
| 0.10 | 88% | 99.9% |

Default `min_margin` of 0.10. An operator who wants everything classified sets
it to 0.

### Operators are told which of their classes work

This is what makes an unbounded class count safe, and it is the part that turns
"how many classes should I have" from an opinion into a measurement.

`POST /admin/prompt-classes/evaluate` runs leave-one-out over the operator's own
examples and returns per-class precision, recall, support and mean margin, plus
the confusion pairs. A class scoring 30% precision is visibly a bad class, and
the operator can merge it, rename it, or add examples — rather than discovering
the problem as mysteriously misrouted traffic.

`POST /admin/prompt-classes/classify` takes a prompt and returns the full
ranking with scores, for interactive tuning.

Both are control-plane only: they load the model, which the data plane does too
but for a different purpose, and neither is on any request path.

### Where the model comes from

`potion-code-16M` (61 MB) embedded in the image via the same `rust-embed`
mechanism the management UI already uses, behind a `classifier` cargo feature
so a build that does not want it does not carry it. `--classifier-model <path>`
overrides, for an operator who wants a different one.

Feature-gated because the whole point is that this is optional: with the
feature off, `Protocol`-style dead code elimination keeps `model2vec-rs` out of
the tree entirely, and the default build stays what it is today.

### Caching

Key the classification on the same prefix hash already computed for affinity,
in a small bounded LRU. A multi-turn conversation classifies once. This is an
optimisation, not a correctness requirement — the uncached path is ~140µs.

### Observability

Every classified request logs the class and margin at debug, and
`/metrics` gains `fastllm_prompt_class_total{class,outcome}` where outcome is
`matched` or `below_floor`. Without this, a class whose quality degrades as
traffic drifts is invisible until someone complains about answers.

## What this deliberately does not do

- **No difficulty routing.** See above.
- **No per-request class override in the body.** The `headers` condition
  already covers "the client knows better", and two mechanisms for the same
  thing is one too many.
- **No online learning.** Centroids change when examples change, never because
  of traffic. A router that drifts on its own is one nobody can reason about.

## Testing

- Unit: centroid arithmetic, margin computation, floor behaviour, and that a
  below-floor result falls through to the next rule rather than erroring.
- The `bench/potion-real` numbers are the accuracy evidence; the test suite
  pins *mechanism*, not model quality, so it stays fast and deterministic.
- End to end: a class whose examples are coding prompts routes a coding prompt
  to one model and a chat prompt to another, through the real binary.
- `tests/no_io_on_hot_path.rs` extended: classification is CPU-only and must
  not acquire a handle or await.

## Documentation

`README.md` (classes, the rule condition, the evaluate endpoint, and the
per-class floor with the measured coverage table), `docs/architecture.md` (the
classifier sits in the same box as rule evaluation, fed by centroids that
arrive in the snapshot), `deploy/README.md` (image size, the cargo feature,
tuning a class), `TODO.md`.
