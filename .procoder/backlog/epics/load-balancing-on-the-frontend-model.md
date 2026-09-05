# Load balancing on the frontend model

Status: done 2026-09-05
Created: 2026-09-04
Milestone: providers-become-records
Issue: #11

## Description

`migrations/0028_model_routing_policy.sql` opens with its own premise — "how to
choose between one backend model's replicas" — and adds `policy` to `models`.
Once a provider model has exactly one provider it has no replicas, and the
column is stranded. It moves up to the frontend model, where the multiple
targets now live.

The mechanism is mostly already there and unlabelled. `VirtualModels.jsx:204`
tells the user "targets weighted and ordered" and renders each weight as a share
of the total; those weights _are_ a load balancer. But spreading traffic across
two providers is currently an emergent property of adding a second target to a
rule, so the screen reads as routing rules and nothing announces that this is
where you do it.

0028's own example is the reason the policy has to be per-pool rather than a
process flag: two local replicas sharing a prefix cache want `cache-affinity`
while three hosted providers of differing speed want `lowest-latency`.

The target picker changes too. After the decomposition a model name no longer
identifies one thing — two Sparks both expose `bge-m3` — so targets are chosen
as provider/model pairs.
