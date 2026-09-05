# Choose targets as provider/model pairs

Status: done 2026-09-05
Created: 2026-09-04
Epic: load-balancing-on-the-frontend-model
Sprint: sprint-3

## Description

As an operator picking what a frontend model routes to, I choose a provider and a model, because a name alone no longer identifies one thing.

Two Sparks both expose `bge-m3`. A target that names only `bge-m3` is ambiguous.

## Acceptance criteria

- [x] The target picker selects a provider and one of its models
- [x] Targets are stored and displayed with their provider
- [x] `POST /admin/routing/dry-run` reports which provider the chosen target belongs to
- [x] Two same-named models on different providers can both be targets of one frontend model, verified on kw

## Evidence

- Both target pickers show `model · provider`, because a name alone no longer
  identifies one thing — two hosts serving the same model are two provider
  models.
- Targets are stored and displayed with the provider they want
  (`target_provider_name`), which is what they are bound by.
- A target whose provider model has been deleted still appears, marked
  **unavailable**. The list queries inner-joined `provider_models`, so such a
  target vanished from the API and the UI entirely — defeating the point of
  binding by name, which is that it survives and reattaches.
