# Command-line reference

Every flag and every subcommand. `fastllm-proxy --help` prints the same thing
from the binary you are actually running, which is the version to trust if
this page and it ever disagree.

Most flags also read an environment variable — `FASTLLM_` plus the flag in
upper snake case — and the flag wins where both are given. That is what makes
a container configurable without an entrypoint script. The exceptions are the
tuning knobs that a deployment sets once and a container never overrides:
`--policy`, `--admin-port`, `--snapshot-cache`, `--workers`, `--max-retries`,
`--max-body-mb`, `--pool-max-idle`, `--upstream-timeout`, `--health-interval`
and `--health-timeout` are flags only. `--help` on your own binary is the
authority: it prints `[env: …]` beside every flag that has one.

## Subcommands

Run with no subcommand, it is the gateway. The five subcommands are operator
tools, and each takes its own `--database-url` so none of them has to be issued
alongside `--role`.

| | |
|---|---|
| [`import`](#import) | Seed models, backends and keys from a LiteLLM config |
| [`set-password`](#set-password) | Create or reset an admin login |
| [`sync-prices`](#sync-prices) | Fill in model prices from a published catalogue |
| [`reencrypt-backends`](#reencrypt-backends) | One-shot migration of pre-encryption credentials |
| [`classify-bench`](#classify-bench) | Measure the classifier where it actually runs |

### `import`

```bash
fastllm-proxy import --config litellm_config.yaml --database-url postgres://...
```

The migration path off a file-driven deployment. Idempotent: run it once per
environment, or again after editing the file — grants dropped from the file are
revoked rather than left behind, so re-importing converges instead of
accumulating.

It seeds `models`, `model_backends` **and the `auth:` block**: a
`service_account` principal per key, the key as a SHA-256 hash, and its model
grants as a role named `import:<name>`. `models: ['*']` becomes `model:invoke`
on `model/*`; a named list becomes one grant per model.

It never prints a key back. The config file remains the only copy of any
plaintext.

### `set-password`

```bash
fastllm-proxy set-password --name you --password 'change-me' \
  --database-url postgres://...
```

The one gap the admin API cannot close on its own. `PUT
/admin/principals/{id}/password` is gated behind a session cookie, and a
freshly migrated database has no session anyone can obtain — so this is how the
first login gets one, run by whoever already holds cluster access.

Creates the principal if the name is new, and grants it `admin` **unless it
already holds a role granting `config:write`**. Checking for the permission
rather than for "any role at all" is deliberate: the seeded `bootstrap`
principal already holds `inference`, and a laxer check would turn it into an
account that can log in and administer nothing.

Safe to run again to reset a forgotten password. `--password` also reads
`FASTLLM_BOOTSTRAP_PASSWORD`, which is the form to use if your shell history is
somewhere you would rather a password was not.

### `sync-prices`

```bash
fastllm-proxy sync-prices --database-url postgres://... --dry-run
```

| | |
|---|---|
| `--source` | `open-router`, `catalogue`, or `both` (default) |
| `--overwrite` | Replace prices already set, not only fill in the missing |
| `--dry-run` | Report what would change and write nothing |

Only touches models whose price is unset unless `--overwrite`: an operator who
entered a negotiated rate should not have it replaced by a list price on the
next run.

This is the *fallback* source. Where a provider reports what it actually
charged — OpenRouter returns `usage.cost` unasked — that figure wins and
nothing here competes with it.

### `reencrypt-backends`

```bash
fastllm-proxy reencrypt-backends --database-url postgres://...
```

One-shot migration for `model_backends.upstream_api_key` rows still holding
pre-encryption plaintext. Safe to run more than once; an already-encrypted row
is left alone. Needed once, by deployments that predate
`migrations/0004_encrypted_upstream_api_key.sql`.

### `classify-bench`

```bash
kubectl exec deploy/fastllm-proxy -- fastllm-proxy classify-bench --concurrency 8
```

| | |
|---|---|
| `--iterations` | Default 20 |
| `--concurrency` | Default 4 — so the refined tier's session mutex is visible rather than inferred |

It ships inside the image on purpose. The refined-tier cost in
[the classifier chapter](classifier.md) was first measured on a laptop and the
deployed container turned out to be more than an order of magnitude slower.
Guessing at why — thread counts, core quotas, token windows — is what this
exists to stop: it measures the pod's real CPU quota rather than a developer's
machine.

## Flags

### Roles and planes

| flag | default | |
|---|---|---|
| `--role` | `proxy` | `all`, `control`, or `proxy`. See [Roles](operations/configuration.md#roles) |
| `--database-url` | — | Required by `all` and `control`; unused by `proxy` |
| `--control-url` | — | Control plane to poll in `proxy` mode. Absent means `File` mode |
| `--proxy-token` | — | Presented to a control plane by `proxy`; required of callers by `all`/`control` |
| `--snapshot-cache` | `/var/lib/fastllm/snapshot.json` | Last-known-good snapshot, so a control-plane outage degrades to "stops learning about changes" rather than "stops serving" |
| `--admin-port` | `4001` | Admin API bind port (`all`/`control`) |
| `--snapshot-rebuild-interval` | `5` | Seconds between control-plane rebuilds independent of admin writes |
| `--rate-limit-reconcile-interval` | `5` | `Http`-mode `proxy` only. `0` disables |

### Listener

| flag | default | |
|---|---|---|
| `--host` | `127.0.0.1` | Loopback deliberately — binding `0.0.0.0` is an act, not an accident. The Docker image sets it |
| `--port` | `4000` | The gateway |
| `--config` | — | LiteLLM-format config. Required in `File` mode; elsewhere only for the `fastllm:` tuning block |
| `--max-body-mb` | `64` | Largest request body accepted |
| `--workers` | core count | Worker threads |
| `--tls-cert` / `--tls-key` | — | PEM chain and key for the admin listener. Absent means plain HTTP — legitimate for a dev deployment with no real backend credentials, and not otherwise, because `/snapshot` carries usable ones |
| `--ca-bundle` | — | Extra CAs trusted alongside the system roots. The normal case for an in-cluster cert-manager certificate |

### Routing and upstreams

| flag | default | |
|---|---|---|
| `--policy` | `cache-affinity` | Also `least-loaded`, `round-robin`, `lowest-latency`. See [tuning affinity](operations/configuration.md#tuning-affinity) |
| `--upstream-timeout` | `120` | Seconds to wait for response **headers**. Does not bound generation — a long completion is not a hung request |
| `--max-retries` | `2` | Alternate backends tried when one fails **before any bytes are sent**. After the first byte there is nothing to retry onto without lying to the client |
| `--pool-max-idle` | `256` | Idle upstream connections kept per backend |
| `--health-interval` | `10` | Seconds between health sweeps |
| `--health-timeout` | `3` | Seconds a probe may take before it counts as a failure |
| `--health-report-interval` | `10` | Seconds between health reports to the control plane. Backend health exists only in the data plane, so this is the only way the UI can see it |
| `--config-poll` | `5` | Seconds between snapshot refreshes. `0` disables the watch; in `File` mode `SIGHUP` is then the only reload |

### Cache

| flag | default | |
|---|---|---|
| `--cache-max-entries` | `4096` | |
| `--cache-max-bytes` | `67108864` | |

Both matter, and neither alone is enough: a thousand embedding responses is
nothing and a thousand completions is hundreds of megabytes, so a single
ceiling leaves the other dimension unbounded. Only models that turn caching on
ever reach them.

### Observability

| flag | default | |
|---|---|---|
| `--log` | `info` | |
| `--log-format` | `text` | `json` for a log collector |
| `--webhook-url` | — | POSTs JSON when a backend goes down or recovers, or a snapshot rebuild fails. `all`/`control` only — these are things the control plane learns |
| `--webhook-secret` | — | Signs each body with HMAC-SHA256 in `x-fastllm-signature` |
| `--otel-endpoint` | — | Requires `--features otel` |
| `--otel-sample-one-in` | `100` | Tracing every request on a hot path is its own performance problem |

### Classifier

| flag | default | |
|---|---|---|
| `--classifier-model` | image path | Fast tier. Requires `--features classifier` |
| `--classifier-tier2-model` | image path | Refined tier. Requires `--features classifier-tier2` |

### Shutdown

| flag | default | |
|---|---|---|
| `--shutdown-grace` | `25` | Seconds to let in-flight requests finish after `SIGTERM`. Kubernetes `SIGKILL`s at `terminationGracePeriodSeconds` (30 by default), so this sits under it. `0` exits immediately |

## Secrets that are not flags

Three values are environment-only, because a flag ends up in a process listing
and these should not:

| | |
|---|---|
| `FASTLLM_ENCRYPTION_KEY` | Encrypts `model_backends.upstream_api_key` at rest. **Not regenerable** — lose it and those credentials are unrecoverable; change it without running `reencrypt-backends` and the process will not start |
| `FASTLLM_PROXY_TOKEN` | Also a flag, but the variable is the form to use |
| `FASTLLM_BOOTSTRAP_PASSWORD` | `set-password`'s `--password` |

## Where next

| | |
|---|---|
| [Operations](operations.md) | The five deployment shapes, and where each flag lands |
| [API and administration](api.md) | Every HTTP endpoint |
| [Architecture](architecture.md) | What the roles actually do |
