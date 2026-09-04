# Choose targets as provider/model pairs

Status: open
Created: 2026-09-04
Epic: load-balancing-on-the-frontend-model
Sprint: sprint-3

## Description

As an operator picking what a frontend model routes to, I choose a provider and a model, because a name alone no longer identifies one thing.

Two Sparks both expose `bge-m3`. A target that names only `bge-m3` is ambiguous.

## Acceptance criteria

- [ ] The target picker selects a provider and one of its models
- [ ] Targets are stored and displayed with their provider
- [ ] `POST /admin/routing/dry-run` reports which provider the chosen target belongs to
- [ ] Two same-named models on different providers can both be targets of one frontend model, verified on kw

## Evidence
