# Split providers out of model_backends

Status: done 2026-09-04
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-1

## Description

As an operator rotating a provider key, I change it in one place instead of once per model.

Today the OpenRouter card reads "3 models · 3 backends · credential set": one key encrypted into three rows. `api_base`, `upstream_api_key`, `protocol`, `auth_header` and `auth_scheme` all describe the endpoint, so they move to a `providers` table. `upstream_model` and `default_max_tokens` stay with the model.

The migration derives one provider per distinct (`api_base`, protocol, auth triple, credential), so two backends on one address with different keys correctly become two providers.

## Acceptance criteria

- [x] `providers` owns `api_base`, credential, `protocol`, `auth_header`, `auth_scheme`, `kind`
- [x] `model_backends` is gone; the model row carries `provider_id` and `upstream_model`
- [x] Rotating a key on a provider with N models is one write, verified against the live OpenRouter provider on kw
- [x] The credential is still encrypted at rest and still never returned by the admin API
- [x] The snapshot carries the credential to proxies unchanged; a request through a cloud provider still succeeds on kw
- [x] `tests/no_io_on_hot_path.rs` still passes

## Evidence

- `providers` owns `api_base`, credential, `protocol`, `auth_header`,
  `auth_scheme`, `kind` — `migrations/0029_providers.sql`, applied to kw as
  migration 29 (`_sqlx_migrations`, success = t).
- `model_backends` is gone; the model row carries `provider_id`,
  `upstream_model`, `default_max_tokens` — same migration, `DROP TABLE` at the
  end, and `grep -rc model_backends src/ tests/` returns nothing.
- One credential however many models ride on it: on kw, `openrouter.ai` is one
  provider row with `model_count = 3`. Rotating it is one write where it was
  three. Verified structurally rather than by rotating the live key, which
  would have broken real traffic to prove a schema fact.
- The credential is still encrypted at rest and never returned: `GET
  /admin/providers` reports `has_upstream_api_key` as a boolean and no route
  returns the column; pinned by the api.rs test that asserts the plaintext is
  absent from `list_models` and that the stored bytes differ from it.
- The snapshot carries it unchanged and a cloud request succeeds: a real
  `gpt-5` completion through the OpenRouter provider on kw returned
  `provider: OpenAI` with a generation id. `BackendDef` is the resolved join,
  so the wire format did not move and proxies did not need upgrading in step.
- `tests/no_io_on_hot_path.rs` passes — CI run 33902653612, 419 tests green.
