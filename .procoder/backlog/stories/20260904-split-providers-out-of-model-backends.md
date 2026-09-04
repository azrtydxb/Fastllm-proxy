# Split providers out of model_backends

Status: open
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-1

## Description

As an operator rotating a provider key, I change it in one place instead of once per model.

Today the OpenRouter card reads "3 models · 3 backends · credential set": one key encrypted into three rows. `api_base`, `upstream_api_key`, `protocol`, `auth_header` and `auth_scheme` all describe the endpoint, so they move to a `providers` table. `upstream_model` and `default_max_tokens` stay with the model.

The migration derives one provider per distinct (`api_base`, protocol, auth triple, credential), so two backends on one address with different keys correctly become two providers.

## Acceptance criteria

- [ ] `providers` owns `api_base`, credential, `protocol`, `auth_header`, `auth_scheme`, `kind`
- [ ] `model_backends` is gone; the model row carries `provider_id` and `upstream_model`
- [ ] Rotating a key on a provider with N models is one write, verified against the live OpenRouter provider on kw
- [ ] The credential is still encrypted at rest and still never returned by the admin API
- [ ] The snapshot carries the credential to proxies unchanged; a request through a cloud provider still succeeds on kw
- [ ] `tests/no_io_on_hot_path.rs` still passes

## Evidence
