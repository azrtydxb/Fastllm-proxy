---
name: fastllm-backends
description: Run and troubleshoot the inference backends on the DGX Spark pair that FastLLM proxies to — starting or stopping models with vLLM, SGLang or sparkrun, choosing memory and speculative-decoding settings for GB10, and diagnosing a model that will not load, returns empty responses, or serves corrupted output. Use when a Spark node is unresponsive, a model endpoint is down, throughput looks wrong, or a container will not start. Not for FastLLM's own routing or API (fastllm-routing, fastllm-operate).
---

# DGX Spark inference backends

Two nodes: `dgx-spark` (192.168.10.246) and `dgx-spark2` (192.168.10.245),
GB10 / SM121 / aarch64, **121 GB unified memory** shared between CPU and GPU,
joined by a direct ConnectX-7 link on `192.168.10.47.x` (`enp1s0f0np0`; the `f1`
ports are down on this pair).

## The memory rule

**`--gpu-memory-utilization 0.45` is load-bearing, not conservative.** On unified
memory the GPU budget and the OS share one pool. Raising it to 0.80 made a node
answer ping while SSH and the API both timed out — the box thrashes and needs a
power cycle. Measured: at 0.45, 72 GB used / 48 GB available while serving.

Symptom to recognise: **ping works, SSH hangs at banner exchange.** That is
memory starvation, not a network fault.

## sparkrun

- `sparkrun stop <id>` **without `--cluster` silently fails** — it loses the
  cluster's SSH user, prints `Permission denied` *and* `Workload stopped` in the
  same output, and the workload keeps running. Always pass `--cluster`.
- Upstream images need `executor_config: {entrypoint: ""}` in the recipe;
  sparkrun launches containers as `<image> bash -c "<bootstrap>"` and an image
  whose `ENTRYPOINT` is `["vllm","serve"]` parses that as model arguments.
- Prefix the serve command with `cd /cache/huggingface &&` — the image's WORKDIR
  belongs to its own user and multiprocessing children die on `os.chdir`.
- Redirect `HOME`, `VLLM_CACHE_ROOT` and `TRITON_CACHE_DIR` into the mounted HF
  cache, or compile caches are unwritable and are lost every start.
- Node count derives from `tp * pp`, so a `tp=1` job runs `_solo` on one host
  regardless of `min_nodes`. Data parallelism means launching one job per node
  plus a router.
- `sparkrun run` can hang indefinitely on a half-closed Hugging Face connection,
  printing nothing under `--no-follow`. Kill and retry.

## Diagnosing bad output

**Separate the model from the plumbing before blaming either.** `/v1/completions`
bypasses both the chat template and the reasoning parser:

```bash
curl -s http://<node>:8000/v1/completions -H 'content-type: application/json' \
  -d '{"model":"<m>","prompt":"<|im_start|>user\nhi<|im_end|>\n<|im_start|>assistant\n<think>\n","max_tokens":200}'
```

If raw output is good but chat completions are empty, the parser or template is
eating it — not the model. A chat response with `finish_reason: length`, zero
content **and** zero reasoning while `completion_tokens` is at the cap means an
unclosed thinking block.

**Reasoning field names differ by engine**: vLLM emits `reasoning`, SGLang emits
`reasoning_content`. Reading the wrong key reports zero on a field that does not
exist — check both before concluding a model is not thinking.

**A chat template that pre-fills an open `<think>` tag** must be paired with a
parser that splits on the closing tag alone. Pairing it with one that expects an
opening tag in the *output* discards everything.

## Speculative decoding

Throughput is `accept_length × steps/s`. Both matter, and acceptance is
dominated by content, not by configuration — measured 0.19 on prose with
reasoning versus 0.95 on code, on the same server and config. Quote a tok/s
figure only alongside the workload it was measured on.

Draft depth is a property of the drafter, not a tuning knob: a block drafter
trained at K=7 loses accuracy when run at K=5, rather than simply drafting less.
