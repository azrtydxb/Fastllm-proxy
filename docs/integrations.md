# Connecting a client

Everything here is an OpenAI-compatible endpoint, so anything that talks to
OpenAI talks to this. What follows is the exact configuration for the clients
people actually point at it, so nobody has to work it out from first
principles.

Two constants throughout:

| | |
|---|---|
| **Base URL** | `http://<host>:4000/v1` — the data plane. **Not** the admin port |
| **API key** | a key minted through the admin API or the UI, `sk-…` |

The admin port (4001) serves the management UI, `/admin/*` and `/snapshot`. No
client should ever be pointed at it.

## The OpenAI SDKs

```python
from openai import OpenAI

client = OpenAI(base_url="http://gateway:4000/v1", api_key="sk-...")
client.chat.completions.create(
    model="my-model",
    messages=[{"role": "user", "content": "hi"}],
)
```

```javascript
import OpenAI from "openai";

const client = new OpenAI({ baseURL: "http://gateway:4000/v1", apiKey: "sk-..." });
await client.chat.completions.create({
  model: "my-model",
  messages: [{ role: "user", content: "hi" }],
});
```

Streaming, tool calling, `response_format`, images and audio all work as they
do against OpenAI — an `openai`-protocol backend's body is forwarded
unmodified in both directions, so anything the upstream supports survives the
trip.

## Coding agents

These are the ones worth spelling out, because each expects its own file.

### opencode

`~/.config/opencode/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "fastllm": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "fastllm",
      "options": {
        "baseURL": "http://gateway:4000/v1",
        "apiKey": "{env:FASTLLM_API_KEY}"
      },
      "models": {
        "my-model": { "name": "My Model" }
      }
    }
  }
}
```

List only the models that key may invoke. `/v1/models` is filtered by the
caller's grants, but opencode builds its picker from *this file*, so a model
listed here that the key cannot use fails when selected.

### Cursor

Settings → Models → *Override OpenAI Base URL* with `http://gateway:4000/v1`,
and paste the key as the OpenAI API key. Cursor verifies the key by calling
`/v1/models`, so the key needs a grant on at least one model or verification
fails.

### Continue (`~/.continue/config.json`)

```json
{
  "models": [
    {
      "title": "fastllm",
      "provider": "openai",
      "model": "my-model",
      "apiBase": "http://gateway:4000/v1",
      "apiKey": "sk-..."
    }
  ]
}
```

### Aider

```bash
export OPENAI_API_BASE=http://gateway:4000/v1
export OPENAI_API_KEY=sk-...
aider --model openai/my-model
```

### Zed (`settings.json`)

```json
{
  "language_models": {
    "openai": {
      "api_url": "http://gateway:4000/v1",
      "available_models": [{ "name": "my-model", "max_tokens": 262144 }]
    }
  }
}
```

## Frameworks

### LangChain

```python
from langchain_openai import ChatOpenAI

llm = ChatOpenAI(
    base_url="http://gateway:4000/v1",
    api_key="sk-...",
    model="my-model",
)
```

### LlamaIndex

```python
from llama_index.llms.openai_like import OpenAILike

llm = OpenAILike(
    api_base="http://gateway:4000/v1",
    api_key="sk-...",
    model="my-model",
    is_chat_model=True,
)
```

`OpenAILike` rather than `OpenAI`: the latter refuses model names it does not
recognise from OpenAI's own catalogue, and yours will not be in it.

### Vercel AI SDK

```typescript
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";

const fastllm = createOpenAICompatible({
  name: "fastllm",
  baseURL: "http://gateway:4000/v1",
  apiKey: process.env.FASTLLM_API_KEY,
});
```

## Embeddings and rerank

Same endpoint, same key, subject to the same per-model grants:

```python
client.embeddings.create(model="bge-m3", input="hello")
```

```bash
curl http://gateway:4000/v1/rerank -H "authorization: Bearer sk-..." \
  -H 'content-type: application/json' \
  -d '{"model":"bge-reranker-v2-m3","query":"q","documents":["a","b"]}'
```

`/v1/rerank` and `/v1/score` are forwarded like any other POST carrying a
`model`; they are not OpenAI endpoints, but every engine that implements them
uses the same shape.

## Images

```python
client.images.generate(model="dall-e-3", prompt="a red bicycle", size="1024x1024")
```

```bash
curl http://gateway:4000/v1/images/generations -H "authorization: Bearer sk-..." \
  -H 'content-type: application/json' \
  -d '{"model":"dall-e-3","prompt":"a red bicycle","size":"1024x1024"}'
```

`/v1/images/edits` too. Binary and base64 responses go through the same byte
pump as a token stream — nothing on the response path parses a passthrough
body, whatever is in it.

## Speech

Text to speech, and both directions of transcription:

```python
client.audio.speech.create(model="tts-1", voice="alloy", input="hello")
client.audio.transcriptions.create(model="whisper-1", file=open("clip.mp3", "rb"))
client.audio.translations.create(model="whisper-1", file=open("clip.mp3", "rb"))
```

The transcription endpoints take a multipart upload, and the client's
`content-type` is carried through untouched — the boundary parameter lives in
that header, and rewriting it would make the body unparseable upstream.

Each of these is the same one-row configuration as a chat model, authorised by
the same per-model grant and counted in the same usage accounting.

## Observability

### Prometheus

`/metrics` on the data plane port, unauthenticated, no scrape config needed
beyond pointing at it:

```yaml
scrape_configs:
  - job_name: fastllm-proxy
    static_configs:
      - targets: ["gateway:4000"]
```

Per-backend health, in-flight, request and error counts, latency histograms,
cache counters, classifier timings and the snapshot version. A ready-made
Grafana dashboard is in [`examples/grafana-dashboard.json`](../examples/grafana-dashboard.json).

### OpenTelemetry

Built with `--features otel`, then `--otel-endpoint http://collector:4317`.
Sampling is one-in-N via `--otel-sample-one-in`, because tracing every request
on a hot path is its own performance problem.

### Webhooks

`--webhook-url` POSTs JSON when a backend goes down or recovers, or when a
snapshot rebuild fails. `--webhook-secret` signs it with HMAC-SHA256 in
`x-fastllm-signature`. A minimal receiver that verifies the signature is in
[`examples/webhook-receiver.py`](../examples/webhook-receiver.py).

### Per-caller detail

Prometheus deliberately does not carry per-principal labels — that cardinality
is how a metrics endpoint becomes an outage. "Which caller got slow" is a SQL
question against `usage_events`, or the **Usage & spend** screen in the UI.
See [operations.md](operations/usage-records.md#per-request-records).

## Coming next

The stateful job APIs — `/v1/batches`, `/v1/files`, `/v1/fine_tuning` and the
Assistants API — and provider-native passthrough paths such as
`/vertex-ai/...`.

Both need one new idea rather than one more route. Everything served today
carries a `model` in its body, which is what the router routes on and what the
per-model grant is checked against. A `GET /v1/files/{id}` carries neither, so
supporting it means the gateway remembering which backend owns which id, and
authorising on something other than a model. That is a design worth doing
properly — an id-to-backend map that survives restarts, and grants that can be
expressed per backend rather than per model — and it is on the list.

Until then, call those endpoints against the provider directly; everything
your application does per request goes through here.

