---
name: fastllm-classifier
description: Manage FastLLM prompt classes for semantic routing — create classes and their example prompts, list or delete them, and evaluate how a given prompt would be classified. Use when routing should depend on what a request is about rather than who sent it, when tuning classifier accuracy, or when a class-based routing rule is not matching as expected.
---

# FastLLM classifier

## Auth

Admin endpoints need a **session cookie**, not a bearer token — the gateway
master key is not an admin credential.

```bash
curl -sk -c /tmp/ck -X POST https://192.168.10.129:4001/login \
  -H 'content-type: application/json' -d '{"name":"<user>","password":"<pw>"}'
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/...
```

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `GET` | `/admin/prompt-classes` | Read prompt-classes | — |
| `POST` | `/admin/prompt-classes` | Create prompt-classes | `name`, `description`*, `tier`*, `min_margin`*, `refines`*, `examples`* |
| `POST` | `/admin/prompt-classes/evaluate` | Leave-one-out precision and recall over your own class examples | — |
| `DELETE` | `/admin/prompt-classes/{id}` | Delete prompt-classes id | — |
| `POST` | `/admin/prompt-classes/{id}/examples` | Create prompt-classes id examples | `prompt` |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**Evaluate before you route on it.** `POST /admin/prompt-classes/evaluate`
returns the class and its margin for a prompt without changing anything. A class
that looks obvious to a human may not clear the confidence floor.

**An unclassified request has no class**, so every rule naming one falls through
— that is the designed behaviour when no classifier model is loaded, no classes
are configured, or the best class misses the margin.

**Classification costs time on the request path.** The fast tier runs per
request; the refined tier is loaded lazily and only when a rule needs it.
