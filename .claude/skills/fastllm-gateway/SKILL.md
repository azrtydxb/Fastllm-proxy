---
name: fastllm-gateway
description: Send inference requests through the FastLLM OpenAI-compatible gateway — chat completions, completions, embeddings, rerank, score, responses, moderations, audio speech and transcription, image generation and edits, and listing available models. Use when calling a model through the proxy, testing that a model or frontend model actually serves, or debugging a 401, 404 or 503 from a client.
---

# FastLLM gateway

## Auth

The gateway takes a **principal API key** as a bearer token — a different
credential from the admin session. Keys are stored SHA-256 hashed and cannot be
read back from the database; if you do not have one, you cannot call `/v1/*`.

```bash
curl http://192.168.10.125/v1/chat/completions -H "Authorization: Bearer <key>" \
  -H 'content-type: application/json' -d '{"model":"<name>","messages":[...]}'
```

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `POST` | `/v1/audio/speech` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/audio/transcriptions` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/audio/translations` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/chat/completions` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/completions` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/embeddings` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/images/edits` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/images/generations` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `GET` | `/v1/models` | Models this key may invoke. Filtered by the caller's grants | — |
| `POST` | `/v1/moderations` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/rerank` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/responses` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |
| `POST` | `/v1/score` | Proxied to the backend serving `model`. Forwarded byte-for-byte for an `openai` backend | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**`401` means the gateway is healthy.** It reached the proxy and was rejected for
credentials. A connection refused or `000` is the failure worth chasing.

**`404 model_not_found` on a frontend model means no viable target**, not an
unknown name — check the chain resolves to something routable.

**An unknown model name is a 404 regardless of permissions**, deliberately, so
"403 vs 404" cannot be used to probe which models exist.

**Reasoning field names differ by backend engine.** vLLM emits `reasoning`;
SGLang emits `reasoning_content`. A client hardcoded to one shows blank reasoning
against the other — check both before concluding a model is not thinking.
