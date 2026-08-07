# Multi-provider support

**Goal.** Reach every provider `genai` reaches, using this repo's own client,
without giving up the property that makes the proxy worth running: a
byte-exact, non-parsing forwarding path.

**Scope.** Group A (OpenAI-compatible providers — configuration only), Group B
(arbitrary auth headers), and native translation for Anthropic and Gemini.
Cohere, Bedrock SigV4 and Vertex OAuth2 are explicitly out.

## The shape of the problem

Of `genai`'s 30 adapters, 21 are an endpoint URL plus `Authorization: Bearer`.
The proxy already forwards the client's path verbatim (`proxy.rs:115` strips
`/v1` and re-appends the subpath to `api_base`), already speaks TLS, already
sets a bearer header. Those 21 — OpenRouter among them — need no Rust at all;
they are rows in `backends`. `genai` hardcodes that table, we accept it at
runtime through the admin API, which is the better end of the trade.

Two more (Gemini, MiniMax) differ only in the *name* of the auth header. Five
speak a genuinely different protocol. So the work divides sharply, and the
cost is concentrated in the last group.

## What must not change

The measured overhead against a real vLLM is zero (TTFT 83.2ms proxied vs
83.8ms direct). That comes from forwarding response bytes without parsing them
and reading a bounded tail once at end of stream. Translation is the opposite
of that.

So: **translation is opt-in per backend, and the passthrough path stays byte
exact.** A backend is `protocol = 'openai'` unless an operator says otherwise,
and on that path not one additional byte is examined. A test pins it: an
`openai` backend's response bytes arrive at the client identical to what the
upstream emitted. (Implemented as
`a_passthrough_backend_is_forwarded_byte_for_byte` in
`tests/native_protocols.rs` rather than the separate
`tests/passthrough_is_byte_exact.rs` this spec first proposed — it needs the
same control-plane-and-mock-provider harness as the translation tests, and
splitting it into its own file would have meant duplicating all of it.)

This is a second execution mode, not a filter every request passes through.

## Group B: auth that isn't `Authorization: Bearer`

`registry::Backend::new` bakes `format!("Bearer {key}")` into an
`authorization` header, so neither the header name nor the scheme can vary.
Gemini wants `x-goog-api-key: KEY`, Anthropic wants `x-api-key: KEY` plus a
constant `anthropic-version: 2023-06-01`, MiniMax's messages endpoint 401s on
Bearer.

Replace `Backend.auth: Option<HeaderValue>` with
`Backend.headers: Vec<(HeaderName, HeaderValue)>` — built once at snapshot
build, applied with a loop in `build_upstream_request`. The hot path gains one
short iteration over a two-element vector and loses a branch; nothing is
computed per request that was not computed before.

Storage: `backends.auth_header` (default `authorization`) and
`backends.auth_scheme` (default `Bearer`, nullable for raw-key schemes). The
protocol adapter contributes its own constant headers (`anthropic-version`),
so an operator never types those.

## Group C: native translation

### Where it lives

A new `src/protocol/` module, one file per wire format:

- `mod.rs` — the `Protocol` enum, the dispatch, the shared OpenAI-shaped types
- `anthropic.rs` — Messages API
- `gemini.rs` — `generateContent` / `streamGenerateContent`

Nothing in `protocol/` is reachable from a `Protocol::OpenAI` request.

### Request translation

An extension of what `rewrite_model_if_needed` already does — that function
re-serializes the body whenever a model alias is in play, so the request side
already tolerates a rewrite. Translation replaces that call for native
backends, and consumes the same already-collected `Bytes`.

Mapping, OpenAI chat/completions → Anthropic Messages:

| OpenAI | Anthropic |
|---|---|
| `messages[role=system]` | top-level `system` string |
| `messages[role=user\|assistant]` | `messages[]`, same roles |
| `max_tokens` | `max_tokens` — **required upstream** |
| `temperature`, `top_p` | same |
| `stop` | `stop_sequences` |
| `stream` | `stream` |
| path `/chat/completions` | path `/messages` |

→ Gemini `generateContent`:

| OpenAI | Gemini |
|---|---|
| `messages[role=system]` | `systemInstruction.parts[].text` |
| `role: assistant` | `role: model` |
| `messages[].content` | `contents[].parts[].text` |
| `max_tokens` | `generationConfig.maxOutputTokens` |
| `temperature`, `top_p` | `generationConfig.temperature`, `.topP` |
| `stop` | `generationConfig.stopSequences` |
| path | `/models/{upstream_model}:generateContent`, or
`:streamGenerateContent?alt=sse` when streaming |

Note Gemini carries the model in the **URL**, not the body — so
`Backend::url_for` grows a protocol-aware variant rather than the caller
string-concatenating a special case.

### The `max_tokens` problem

Anthropic requires `max_tokens`; OpenAI treats it as optional. A request
without one cannot be forwarded unchanged.

Silently inventing a cap is the wrong answer — it truncates generation with no
record of why, which is precisely the class of bug `CLAUDE.md` says this repo
keeps getting bitten by. Instead: `backends.default_max_tokens`, nullable. Set
→ used, and logged once per backend at snapshot build so the value is visible.
Unset → the request fails with a 400 naming the field and the backend. The
operator makes the decision explicitly, once.

### Response translation

The real new machinery, and the only place a response body gets parsed.

**Non-streaming** is a single parse and re-emit. Anthropic's
`content[].text` concatenates into `choices[0].message.content`; `stop_reason`
maps `end_turn|stop_sequence → stop`, `max_tokens → length`; `usage.input_tokens`
/ `output_tokens` become `prompt_tokens` / `completion_tokens`. Gemini's
`candidates[0].content.parts[].text` and `usageMetadata.promptTokenCount` /
`candidatesTokenCount` map the same way; `finishReason` `STOP → stop`,
`MAX_TOKENS → length`, `SAFETY → content_filter`.

**Streaming** needs a stateful `Body` wrapper, because SSE events split across
TCP reads. It holds a partial-event buffer, and per provider:

- Anthropic: `message_start` carries `usage.input_tokens`;
  `content_block_delta.delta.text` becomes a chunk's `choices[0].delta.content`;
  `message_delta` carries `usage.output_tokens` and the final `stop_reason`;
  `message_stop` ends it. `ping` is dropped. Emit a leading role chunk, then
  content chunks, then a finish chunk, then `data: [DONE]`.
- Gemini with `alt=sse`: each event is a full `GenerateContentResponse`; emit
  one chunk per event, take usage from the last one carrying `usageMetadata`.

**Usage comes free in this mode.** The translator has already parsed the
numbers the tail buffer exists to recover, so translated responses report usage
directly and skip `TailBuffer` entirely. This is a simplification, not an extra
cost — and it removes the estimation error on providers whose SSE tail we would
otherwise have to guess at.

### What v1 refuses, loudly

Translation that silently drops a field is worse than no translation, because
the caller cannot tell. Each of these returns a 501 naming the unsupported
feature and the backend's protocol:

- `tools` / `tool_choice` / `functions`
- non-text content parts (images, audio)
- `n > 1` (Anthropic has no equivalent)
- `logprobs`, `seed`, `response_format`
- any `PROXIED_SUFFIXES` entry other than `/chat/completions` — embeddings,
  rerank, audio and completions are not translated

These are the honest boundary of v1, and each is a small, additive follow-up.

## Data model

```sql
-- The table is `model_backends`; this spec first wrote `backends`.
ALTER TABLE model_backends
  ADD COLUMN protocol TEXT NOT NULL DEFAULT 'openai'
    CHECK (protocol IN ('openai', 'anthropic', 'gemini')),
  ADD COLUMN auth_header TEXT NOT NULL DEFAULT 'authorization',
  ADD COLUMN auth_scheme TEXT NULL DEFAULT 'Bearer',
  ADD COLUMN default_max_tokens INTEGER NULL;
```

All four default to today's behaviour, so the migration is a no-op for every
existing row. `Protocol` rides in `WireSnapshot` as a string and parses on
`from_wire`; an unknown value drops that backend from the snapshot with a
logged reason, exactly as an undecryptable key already does — a control plane
newer than a proxy must not be able to make the proxy serve traffic it does not
understand.

## Health checks

**Corrected during implementation.** This section originally assumed Anthropic
had no model-listing endpoint and specified a `POST /v1/messages` probe with
`max_tokens: 1`, on a longer interval because it would cost a token. That was
wrong: Anthropic does serve `GET /v1/models`, and Gemini serves
`GET /v1beta/models`. All three protocols therefore probe the same
`{api_base}/models` path and the prober needs no protocol awareness at all.

What it did need was authentication. The prober sent no headers whatsoever,
which is fine against a self-hosted vLLM and fatal against every hosted
provider: `/models` answers 401, the sweep reads that as "down", and the
backend is taken out of rotation while serving perfectly well. That is a
pre-existing bug — it would have hit an OpenRouter backend added with no code
changes at all — and the fix is to send the backend's pre-built headers.

## Testing

- Table-driven fixtures per provider: a recorded native request/response pair
  in `tests/fixtures/{anthropic,gemini}/`, asserting the exact translated JSON
  both ways. No mocks that merely confirm the code called itself.
- SSE re-framing fed **byte by byte** and in pathological splits (mid-event,
  mid-UTF-8) to prove the partial-event buffer holds.
- Every refusal above asserted to 501 with the feature named.
- `tests/passthrough_is_byte_exact.rs` as described.
- End to end on kw against a real provider before any claim that it works, per
  `CLAUDE.md`.

## Documentation

`README.md` (provider table, new backend fields), `docs/architecture.md` (the
request diagram gains the translation branch and must show it as off the
default path), `deploy/README.md` (operator steps for adding a provider),
`TODO.md`. Same commit as the code.
