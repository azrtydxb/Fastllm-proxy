# Registering hosts that serve models

A host that serves models can tell FastLLM so, and keep telling it. When it
stops, its providers stop being routed to — and eventually stop existing.

This exists because the registry drifts. Two real cases, both found by hand:
a provider model pointing at a host that had been swapped to serve something
else entirely, and a model with no provider at all. The first is the
interesting one: the host was **healthy and answering**. No liveness check can
catch that, because nothing was down.

## What the agent does

It registers an **address**, on a lease, and refreshes it. That is all.

It deliberately does not send a model list. FastLLM has to reach the endpoint
anyway in order to serve traffic, so the control plane calls `GET /v1/models`
itself: a list pushed from the host could name models the proxies cannot dial,
and that failure would surface at request time, to a user. Enumerating from the
control plane makes discovery and reachability the same test.

It dials the control plane and is never dialled, so it works from a host behind
NAT, or on a cluster FastLLM cannot reach into.

## Running it

```bash
FASTLLM_CONTROL_URL=https://control.example:4001 \
FASTLLM_AGENT_TOKEN=sk-... \
python3 agent/fastllm-node-agent.py \
  --advertise 192.168.10.246 \
  --scan-ports 8000 8001 8890 \
  --engine vllm
```

Standard library only, so there is nothing to install. That is deliberate: this
runs on machines whose Python is whatever the vendor shipped, and a health
agent that needs a virtualenv to start is one more thing to be broken at 3am.

| Flag | Why it matters |
| --- | --- |
| `--advertise` | The address **proxies** will dial. Configured, never inferred — an agent that discovers a container on `172.17.0.2` and registers that hands the proxies an address they cannot reach. |
| `--scan-ports` | Ports to probe on that address. Catches a bare process started by hand or by a launcher, with no container runtime present. |
| `--api-base` | Register an endpoint outright, repeatable. Use when the address is not a port on `--advertise`. |
| `--ttl` / `--interval` | Lease length and heartbeat. The agent refuses an interval that is not well inside the TTL, since one slow beat would then expire the lease. |
| `--provider-name` | What this host's providers are called in FastLLM. The endpoint's port is appended, so one host's endpoints stay distinguishable — always, not only when a second one appears, since a name that changed shape as a model was started would rename the first one behind you. Defaults to `--node`, and is sent on every heartbeat, so changing it renames them. |
| `--engine` | A hint, carried as metadata. Nothing depends on it. |
| `--token` | A principal API key. It authenticates the agent; there is no permission to grant beyond that. |
| `--ca-cert` | PEM bundle to verify the control plane against, when its certificate comes from an internal CA. There is deliberately no way to skip verification — the token above goes over this connection, and an agent that stops checking hands it to whoever answers. Pass the CA's certificate, or a lone self-signed certificate, which is its own issuer. |
| `--once` | Register and exit, for a cron or a smoke test. |

## Running it under systemd

`agent/fastllm-node-agent.service` is the unit this project's own DGX Sparks
run. It expects the script at `/usr/local/bin/fastllm-node-agent` and its
configuration in `/etc/fastllm/agent.env`:

```ini
FASTLLM_CONTROL_URL=https://control:4001
FASTLLM_AGENT_TOKEN=fllm_...
FASTLLM_NODE=dgx-spark
FASTLLM_ADVERTISE=192.168.10.246
```

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin fastllm-agent
sudo install -m 0755 agent/fastllm-node-agent.py /usr/local/bin/fastllm-node-agent
sudo install -d -m 0755 /etc/fastllm
sudo install -m 0644 ca.crt /etc/fastllm/ca.crt
# The token is a live credential: the agent's user, and nobody else.
sudo install -m 0640 -o root -g fastllm-agent agent.env /etc/fastllm/agent.env
sudo install -m 0644 agent/fastllm-node-agent.service /etc/systemd/system/
sudo systemctl enable --now fastllm-node-agent
```

`Restart=always`, because a host that serves models is not a host that should
stop saying so over one failed registration. The unit has no `After=docker`:
the agent registers whatever is serving, container or bare process, and must
come up on a host with no container runtime at all.

## The name lives on the agent

A dynamic provider is named by the host registering it, not by the control
plane, which only ever sees an address. `dgx-spark-8000` is a better thing to
read on a screen than `192.168.10.246:8000`, and the agent is what knows which
is which.

Renaming is therefore a matter of changing `--provider-name` and letting the
next heartbeat carry it. That is safe: routing resolves a target by its
**model's** name, so a provider's name is descriptive. The rename is carried
onto the targets that describe it, so nothing is left naming a provider that no
longer exists.

A name another provider already holds is declined — with a warning in the
control plane's log — and the heartbeat still succeeds. A collision is not a
reason to let a lease lapse, and an operator seeing the old name is visible and
recoverable in a way that a host which quietly stopped renewing is not.

Static and cloud providers are named the other way round: a cloud provider
takes the vendor's own name from the catalogue (`OpenRouter`, not
`openrouter.ai`), and a static one is named by whoever adds it — on the
Providers screen, or with `PATCH /admin/providers/{id}`.

## There is no RBAC on providers

A provider is an endpoint and a credential for reaching it. The credential is
the *provider's* own — an OpenRouter key, a vLLM server's auth — defined by the
provider rather than by us, and it is the whole of what a provider needs to
work.

Registering one needs a token and nothing more. That is safe because
registering is not an exposure: a model learned from a registered host reaches
nobody until an operator points a frontend model at it, and a frontend model is
where access is actually granted. A permission guarding registration would be
guarding a door that opens onto nothing.

What registration deliberately cannot do is convert a provider a human typed in
into one that expires — a static provider stays static, so an agent cannot take
over an endpoint someone configured by hand.

That leaves a gap worth knowing about: put an agent on a host whose endpoints
were already configured by hand, and it will register them happily and change
nothing. Every line will read `kind=static leased=False`, and the lease, the
degradation and the model reconciliation will all be doing nothing. The
handover is an operator's explicit act:

```bash
curl -sk -b /tmp/ck -X PATCH https://control:4001/admin/providers/4 \
  -H 'content-type: application/json' -d '{"kind":"dynamic"}'
```

and `{"kind":"static"}` takes it back, clearing the lease and any degradation
with it.

## Any engine, in a container or not

Every engine worth naming answers `GET /v1/models` — vLLM, SGLang,
llama.cpp's server, TGI, Ollama, Triton's OpenAI frontend, LM Studio, mlx-lm —
as do the hosted providers. So neither the agent nor the control plane needs to
know which one it found. An unrecognised engine is registered like any other; it
simply contributes no metadata.

There is no container mode. A port probe finds a process however it was started.

## What the control plane does with it

Every `--provider-sweep-interval` (60s by default), one `GET /v1/models` per
provider answers both questions that matter:

- **is it reachable** — the ordinary health question
- **is it still serving what is registered against it** — the drift question,
  which is the one that started all this

A provider serving *more* than is registered is healthy, not drifted. OpenRouter
answers with hundreds of models and three of them are registered; treating the
extras as drift would mark every cloud provider broken.

## Degrade, then delete

A failed probe or a lapsed lease marks the provider **degraded**: its models go
out of rotation, and nothing is deleted. Only after 30 minutes of sustained
absence is a dynamic provider removed.

Two stages, never one. A 27B on a DGX Spark takes over ten minutes to load and
answers nothing while it does, and a host reboot is routine. Deleting on the
first failed probe would make every restart look like a decommissioning — and
deleting a provider throws away its credential. Suppressing routing is
reversible; deletion is not.

> [!IMPORTANT]
> **Static and cloud providers never expire.** They are probed on the same
> schedule, but the result is advisory: it reports drift an operator would
> otherwise find by accident. A human put them there, and absence is not
> evidence the human changed their mind.

## A learned model is inventory, not an exposure

A model that appears on a registered host is registered as a provider model —
and reaches nobody. Nothing creates a frontend model for it.

That is deliberate. Authorisation is granted on frontend models, so a host
starting an unrelated model must not hand existing principals access to it. New
inventory is closed by default; exposing it is a decision someone makes, by
pointing a frontend model at it.

## What survives a provider being deleted

- **Usage and spend.** `usage_events` records the model and provider name at
  ingest, so history outlives the row (migration 0031).
- **Frontend models and their targets.** A target is bound by name, so deleting
  the provider model leaves the target in place, unresolved — and re-registering
  the same model on the same provider reattaches routing with no manual step
  (migration 0036).
- **Grants**, which are held on frontend models rather than on provider models.

Each of those is there because the alternative was demonstrated: the provider
decomposition revoked live grants, would have failed the retention batch
outright, and silently halved a frontend model's capacity — all three found by
deploying and making a real request.
