# Self-registering hosts

Status: open
Created: 2026-09-04

## Goal

A host that serves models tells FastLLM what it is serving, and stops telling it
when it stops. Nobody edits the registry by hand to keep up with a model swap.

The registry drifted twice this week, both fixed by hand. The sharper case was a
provider model pointing at a host that was healthy and answering while serving a
different model entirely — `src/health.rs` probes and rotates, so it can answer
"is this address answering" and can never answer "is this address still what the
row claims".

Reaching this milestone means swapping a model on a Spark is a complete
operation: the old provider model goes, the new one appears, frontend models
reattach on their own, grants keep working, and the week's usage still adds up.

Two things in here are prerequisites rather than follow-ups. Grants are pinned
to concrete rows the registrar would churn, and model deletion cascades usage
history away. Shipping the service before both means every swap either breaks
access, silently widens it, or destroys billing records.

## Done when

- A service on a model host registers its providers on a lease and heartbeats;
  it works on bare processes, in Docker, and on a remote Kubernetes cluster.
- The control plane learns each provider's models itself, so discovery and
  reachability are the same test.
- A provider that stops answering degrades before it is deleted, with a grace
  window longer than a model load.
- Static and cloud providers are probed for drift but never expire.
- Access is granted on frontend models, which are never auto-removed.
- Usage and spend survive a provider model being deleted and its name reused.
- A frontend model whose targets vanish keeps its name, its grants and its
  rules, and says why it is unavailable.
