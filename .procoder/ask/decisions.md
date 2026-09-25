## Which CI build-time fix, if any, should I make?

A one-line CSS change costs ~25 minutes: `test` ~10 min, then `publish`
~12-17 min, run in series because `publish` needs `[test, ui]`. The `ui` job,
the only one that could have failed on that diff, took 1 minute.

Causes, measured: no cache for `~/.cargo` or `target/` (the only `actions/cache`
in the workflow is for the classifier model, and ARC runners are ephemeral
pods), so Rust compiles cold three times per run — clippy, clippy with
features, then test. The Dockerfile then compiles it a fourth time, in release
mode for arm64, with no dependency layer to survive a source change.

Options:

- **Cache cargo and `target/` in the `test` job.** Biggest win for the least
  risk; warm runs drop to a couple of minutes. Helps every run, Rust or not,
  and restructures nothing.
- **Split the Dockerfile's dependency layer** (cargo-chef or a manifest-only
  pre-build) so a source change recompiles this crate rather than the whole
  tree. Touches the image build every deploy depends on.
- **Skip the Rust job when no Rust changed** (`paths-filter`). A web-only
  commit becomes ~1 min plus the image build. Needs a `needs:` restructure,
  not just a skip: the UI is embedded via `rust-embed`, so the image must
  still be rebuilt.
- **Leave it alone.** 25 minutes is tolerable and the pipeline is understood.

**Decided:** cache cargo and `target/` in the `test` job. The other two remain open.

## Deploy the pool-policy fix to kw?

The LB pool feature has a bug: pool policies (e.g. `cache-affinity`,
`least-loaded`) were wired into the failover order but never reached
`Router::pick`, so backends always used the deployment's `--policy` for
dispatch regardless of what the pool configured.

Fix: walk frontend model targets in `build_snapshot`, collect pool→member→policy
mappings, write into `ModelDef.policy` so the registry's `PoolInner.policy` is
set correctly. New test verifies a `least-loaded` pool member gets that policy
in the snapshot. 397 tests pass, gate clean.

## Out-of-band access to the DGX Sparks now that wifi is off

Both nodes are ethernet-only: `piwifi` autoconnect set to `no`, the radio
disabled, and `wlP9s9` down on each. That removes the failure mode where a
wired fault silently demotes an inference node to a 2.4 GHz link and it keeps
serving, slowly, with nothing alerting.

It also removes the only network path in. A switch-port or cable fault on
`enP7s7` now means a console or physical trip, on a pair of boxes that already
need a power cycle when they thrash on memory.

Options:

- **Leave it as done — ethernet only, radio off on both.** Matches the request
  exactly. No emergency path.
- **Re-enable the radio on one node with `ipv4.never-default yes`.** Keeps a
  way in if the wired link dies, while the wifi can never install a default
  route, so it cannot silently carry traffic. Half the fleet stays reachable.
- **Re-enable on both with `never-default`.** Emergency path everywhere, same
  no-default-route guarantee, but two hosts hold a second address on the /24
  again.

**Decided:** leave it as done — ethernet only, radio off on both. Recovery from a
wired fault is a console or physical trip, accepted deliberately.

## Commit and deploy the unhealthy-backend failover fix?

A router-level bug caused a completely dead model to block failover: `Router::pick`
returned an unhealthy backend instead of `None`, so `proxy_request` never moved to
the next model or the deployment-wide fallback. `tests/failover.rs` already covers
the end-to-end path; unit tests all pass and the gate is clean.

Options:

- **Commit and push, then deploy to kw.** Straightforward — the fix is small and
  tested.
- **Defer for now.** No one has filed a bug yet, but the behaviour is silently
  broken and the fix is ready.

**Decided:** commit, push, deploy.

## Pull in the next ChatGPT OAuth story?

Story 1 (admin API accepts `credential_kind: chatgpt_oauth`) is closed. Sprint
`001-add-chatgpt-oauth-provider-credential-kind` is active with 11 remaining
stories. The logical next story is:

- **Task 4 — OAuth connect/status/disconnect APIs** (control plane: PKCE, token
  exchange, encryption, storage)

Or we could continue with the simpler stories first:

- **Task 5 — Rust token refresh** (snapshot build decrypts tokens, refreshes
  if within 5 min of expiry)
- **Task 6 — Health check / error handling** (401/403/429 from OAuth backends
  mark unhealthy → failover; already mostly existing code)
- **Task 7 — Tests** (unit tests for token parsing and refresh timing)

Options:

- **Pull in Task 4 (OAuth APIs) first.** The big piece: new module
  `src/control/oauth.rs`, three new endpoints, full PKCE + token exchange.
  Everything else depends on having tokens to decrypt.
- **Pull in Task 5 (token refresh) first.** Simpler: just a check and exchange
  in the snapshot build. The OAuth connect flow still needs the endpoints, but
  the data plane can start working on receiving tokens.
- **Pull in Task 6 (health/error) first.** Almost no code — just verify existing
  401/403 handling works for OAuth backends. Quick win, builds confidence.
- **Defer for now.** The foundation is laid (DB, schema, API validation). Come
  back when you want to actually connect a ChatGPT account.

- [decision] decisions.md Commit ChatGPT OAuth sprint 002

11 of 12 stories closed. 1 deferred (UI status — backend API ready). Tests pass (392/0). Gate clean (0 blocking).

Options:

- **Commit, push, and deploy to kw.** Sprint complete. The OAuth provider is fully usable on the control plane (connect, token refresh, disconnect, health).
- **Hold for now.** Wait for something else to batch with.

**Decided:**

## Fix four filed GitHub issues before committing sprint changes

Four open issues were raised before the sprint commit:

- **#21** — Headers-timeout failover re-dispatches full body → retry amplification (fixed: timeout-rate breaker + threshold config)
- **#18** — `/v1/models` strips `max_model_len` metadata (fixed: field added through model chain)
- **#19** — Health sweep can't detect vLLM engine-core deadlocks (in progress: needs `/metrics` stall detection)
- **#20** — Per-backend upstream timeout (in progress: needs per-backend config field)

Options:

- **Fix all four now.** They're all in the data plane and orthogonal to the ChatGPT OAuth work that is already committed. The remaining two (#19, #20) touch `registry.rs`, `state.rs`, `proxy.rs`, and the config schema.
- **Defer #19 and #20.** #21 and #18 are done and can be committed separately; #19 requires engine-scrape integration and #20 requires DB migration plus admin API changes, both larger undertakings.

**Decided:** fix all four before committing sprint changes.

## The pool member picker now caps every pool at one member

`3a3b0c2` scoped pool members to "providers of the pool's model". But a pool
member is a `provider_model_id`, and since migration 0045 one model is one row
carrying many backends. So:

- **Create form:** pick a model, and exactly one checkbox appears (verified —
  the interaction harness reports `found 1` and posts a single
  `provider_model_id`).
- **Existing pool:** `available` filters to models whose name matches the
  current member, and that single row is already `inPool` — so the add-member
  list is empty forever.

A one-member pool has nothing to choose between, so pools are currently inert.
The field's own hint ("tick every provider of this model to pool between them")
describes behaviour the schema cannot express: there is no way to name a single
provider as a member.

`6ea7f29` (pool policies reaching `Router::pick`) is sound and passes against
real Postgres — that one is worth deploying either way.

Options:

- **Revert the picker to multi-model, deploy the rest.** Restores pools that
  choose between models, which is what the schema supports and what the docs
  describe. Provider-level balancing within a model stays the model's own
  policy, which `6ea7f29` has just made work.
- **Make members provider-level for real.** A migration so a member can name a
  backend, plus API and UI. Matches the new hint text, but it is a schema
  change and a bigger piece of work.
- **Deploy as-is.** The pool-policy fix lands; pools stay single-member and
  inert until the picker is revisited.

**Decided:** deleted. Its working tree was clean and its HEAD was `dc887bc`,
already on main, so it held nothing that was not already committed.

## Nexus is full, which blocks CI and the deploy

`nexus-data-iscsi` is 200Gi and 100% used — 185.6G of it one `default` blob
store. Nexus logs `No space left on device` and its H2 database has closed, so
every repository returns 500: cargo (CI `test`), npm (CI `ui`), and the Docker
registry. That is the single cause of every CI failure since `4e8bdab`, not
anything in the code.

It also means the cluster cannot pull `fastllm-proxy` images at all — the
manifest for the _currently deployed_ `sha-4e8bdab` 500s. Running pods are fine
on their local cache; a restart onto a cold node would not be.

The image for `f2e8b43` builds cleanly on kw's buildkit — it got as far as
exporting the manifest, and only the push failed.

Options:

- **Expand the PVC.** `truenas-iscsi` has `allowVolumeExpansion=true`, so this
  is an edit plus a Nexus restart. Fastest way back to a working CI and a
  normal deploy, and it does not delete anything.
- **Reclaim space inside Nexus.** Cleanup policies plus "Compact blob store".
  Fixes the cause rather than the symptom, but it deletes artefacts and needs
  decisions about what may go.
- **Bypass the registry for this deploy.** Export the built image from buildkit
  and import it into containerd on the worker nodes, with
  `imagePullPolicy: IfNotPresent`. Gets `f2e8b43` running today, but the image
  exists only on the nodes imported to, so a reschedule elsewhere fails — and
  CI stays broken either way.

**Decided:** expand the PVC. Done — 200Gi → 300Gi, Nexus restarted to complete
the filesystem resize, now 66% used with 94G free. cargo, npm and the registry
all answer again, CI went green, and `sha-f2e8b43` is deployed.

## `npm test` inside the image build keeps exhausting inotify

The `publish` job failed with `EMFILE: too many open files, watch '/web'` —
vite's file watchers hitting `fs.inotify.max_user_instances`, which is at the
kernel default of **128** on the runner node (`max_user_watches` is fine at
249535). A re-run passed, so it is load-dependent rather than deterministic,
and it will recur.

The suite being run there is the same one CI's `ui` job already ran and passed
in the same workflow — the Dockerfile's `RUN npm test` is a second execution of
it, and the only thing that distinguishes it is that it can fail this way.

Options:

- **Drop `RUN npm test` from the Dockerfile.** Removes the duplicate run and
  the failure mode with it. `ui` still gates every commit, so nothing goes
  unverified; the image build stops re-proving what the pipeline just proved.
- **Raise `fs.inotify.max_user_instances` on the runner nodes.** Keeps the
  in-image test as a belt-and-braces check for anyone building the Dockerfile
  outside CI. A node-level sysctl, so it needs to survive node rebuilds.
- **Both.** Drop the duplicate run _and_ raise the limit, so other watcher-heavy
  builds on those nodes stop being fragile too.
- **Leave it.** Re-running the job clears it when it happens.

**Decided:** drop `RUN npm test` from the Dockerfile (`ed15a5d`). CI's `ui` job
still runs the suite on every commit and `publish` needs it, so nothing goes
unverified; a hand-built image is the only thing that loses the check, and the
comment there says so. The sysctl is left alone.

## Pool members should be backends of one model, not the model

The MEMBERS list shows one row per model (`qwen3-6-35b-a3b-nvfp4`, with its two
providers printed as a label). It should show one row per backend —
`…245:8000` and `…246:8000` as separately tickable — so a pool balances across
a chosen subset of one model's providers.

A member is a `provider_model_id` today, and the routing chain is model names
end to end: `expand_pool` returns `Vec<String>`, and the registry keys its
pools by model name. Nothing downstream can name a single backend, so this is
not a UI change.

What it takes, and it is all of these or none:

- a migration adding `model_backend_id` to `model_pool_members`, and a
  uniqueness rule that allows the same model twice with different backends;
- the admin API accepting a backend on the member;
- `build_snapshot` emitting the pool as its own `ModelDef` carrying exactly the
  ticked backends and the pool's policy, so the registry balances within it;
- `expand_pool` returning the pool's own name for such a pool;
- the UI listing backends once a model is chosen.

Checked and clear: `/v1/models` lists frontend models only, and authorisation
is on the client-facing name, so a pool-scoped `ModelDef` leaks into neither.

Two consequences worth deciding on rather than discovering:

- usage rows would attribute to the pool name rather than the provider model,
  because that is what served the request;
- pools of _different_ models (failover between `qwen3.5-9b` and
  `qwen3-8-27b`) and pools of _one model's backends_ become two behaviours in
  one table. They can coexist — a member with a backend means the new one — but
  it is two things to hold in your head.

Options:

- **Build it.** The five pieces above, with tests at each layer.
- **Show the backends, keep the member a model.** After picking a model, list
  its backends read-only so it is visible what the policy will balance across.
  No schema change, and it matches what routing can express today — but you
  cannot exclude one provider.
- **Leave it.** Pools stay model-level; provider balancing is the model's own
  policy, which `6ea7f29` made work.

**Decided:** build it. Members become backends of one model.

## The orphaned `tank/objects` ZFS dataset on NovaNAS

Removing keycloak, openbao, rustfs, nova-api and the observability stack from
NovaNAS left the ZFS dataset `tank/objects` (192K, mounted at `/tank/objects`)
with nothing that references it. It was the object-storage plugin's tier-2
dataset, declared in that plugin's `needs/dataset.yaml` and `rustfs.env`, all of
which are now deleted.

It was left in place deliberately: destroying a ZFS dataset is a different kind
of action from purging packages and config, and it was not part of what the
removal was asked to cover. The same applies to the config backups this session
wrote across `/etc` (`*.bak-<timestamp>`, `*.bak2-<timestamp>`) and
`/etc/hosts.bak-*`, which are now the only record of the pre-removal state.

Options:

- **Destroy the dataset, keep the backups.** `zfs destroy tank/objects`
  reclaims the mountpoint and the 192K. The `/etc` backups stay as a record of
  what the configs looked like before today.
- **Destroy both.** Also delete this session's `*.bak-*` files under `/etc` and
  `/usr/local/bin`. Nothing left to reconstruct the old config from.
- **Leave both.** The dataset costs 192K and an unused mountpoint; the backups
  cost a few hundred KB. Neither is doing harm.

## NovaNAS k3s carries more than the builder it is expected to

The expectation for NovaNAS is "a k3s cluster with a builder, that's it". The
builder is really a small stack — buildkit plus the things it depends on: ARC
runners (github.com/azrtydxb) that drive it, cert-manager issuing its mTLS
certs from the `cluster-ca` issuer, kube-vip supplying the `192.168.10.211`
LoadBalancer address, and openebs zfs-localpv backing its 150Gi cache. Those
all earn their place.

What does not:

- **KubeVirt + CDI** — 10 pods, 41 of the cluster's 51 CRDs, ~740Mi RAM,
  running zero VirtualMachines and zero DataVolumes.
- **csi-nfs** — 2 pods; the `nfs-csi` storage class has no PVCs, and the host
  export `/srv/k8s-nfs` has no consumers.
- **snapshot-controller** — 2 pods for a single 144-day-old VolumeSnapshot,
  `test-fs-snap`, whose source volume is gone.
- **Empty namespaces** — `novanas-apps-system` and `novanas-vms` (left behind
  by the nova platform removed earlier today) and `sera-workspaces`.

Separately, and not a tidiness question: `sera-workspaces` holds a service
account `sera` with a token issued 6 days ago, bound to a cluster-scoped role
`sera-nodes`. Something outside this cluster currently holds credentials to it.

Options:

- **Strip to the builder.** Remove KubeVirt, CDI, csi-nfs, the stale snapshot
  and snapshot-controller, and the empty namespaces. Leaves buildkit, ARC,
  cert-manager, kube-vip, openebs and the k3s baseline.
- **Remove the VM platform only.** KubeVirt and CDI go; csi-nfs and
  snapshot-controller stay in case NFS volumes or snapshots are wanted again.
- **Leave it, now that it is known.** Nothing is broken and the node is at 2%
  CPU / 9% memory.

The `sera` credential is a separate question from any of the above.

**Decided:** leave it as is. The extra components are known and idle, and the
node sits at 2% CPU / 9% memory. The `sera` credential is intentional.

## NovaNAS host is not ready for the R9700 GPUs

Two R9700 cards (RDNA4 / Navi 48, PCI IDs in the `1002:75xx` range) are going
into NovaNAS. ROCm and vLLM are intended to run in containers, which is the
right split — but the kernel driver and its firmware can only live on the host,
and neither is usable today:

- `amdgpu` in the running kernel 6.12.74 carries **no `75xx` PCI IDs** at all
  (its alias table stops around `743F`), so it cannot bind the cards. Debian 13
  ships the 6.12 LTS kernel; RDNA4 enablement is substantially newer.
- `/lib/firmware/amdgpu/` is **empty** — `firmware-amd-graphics` has never been
  installed, though it is available at 20250410-2 from non-free-firmware, which
  is already enabled.
- `trixie-backports` is not configured, so the newest kernel apt offers is
  6.12.90 — still 6.12.

Metrics themselves need nothing on the host: once the driver binds, the kernel
exposes utilisation, VRAM, temperature, power and clocks through sysfs, and
`amd-smi` / AMD's Device Metrics Exporter both run fine in containers with
`/dev/kfd` and `/dev/dri` mapped in. But today's cleanup removed Prometheus,
node-exporter, Grafana and Loki, so there is currently no destination for them.

Options for preparing the host before the cards arrive:

- **Backports kernel + firmware.** Add `trixie-backports`, install its kernel
  and `firmware-amd-graphics`, reboot, confirm the alias table now carries
  `75xx`. Stays on Debian-packaged kernels.
- **AMD `amdgpu-dkms` + firmware.** Keep the 6.12 kernel and build AMD's
  out-of-tree driver from the ROCm repo. Matches ROCm versions exactly, but
  Debian is not an officially supported ROCm distro and DKMS rebuilds on every
  kernel update.
- **Firmware only now, kernel decision at the machine.** Install
  `firmware-amd-graphics` today and leave the kernel until the cards are
  physically in and their exact PCI IDs are readable.
- **Do nothing yet.** Handle it all tomorrow with the hardware present.

Where GPU metrics should land is a separate question: back onto the host, or
in-cluster via the AMD GPU Operator, which also provides the device plugin that
k3s needs in order to schedule the GPUs at all.

**Decided:** not a decision for me — the user is handling the driver/firmware
side themselves. Left here only as a record of what the host looked like the
day before the cards went in.

## ROCm userspace on the NovaNAS host (Debian 13)

Host prep for the two R9700s is done for the parts that are unambiguous:
trixie-backports added, kernel 7.1.8 + headers installed, firmware-amd-graphics
20260810 installed (677 blobs, Navi 48 `gc_12_0_0` present), and zfs-dkms
upgraded 2.3.2 -> 2.4.4 and built for both the running 6.12.74 and the new
7.1.8, so the `tank` pool still imports after the reboot. `atlantic` (the 10G
lifeline) and `igc` both verified present in 7.1.8.

Correction worth recording: the earlier claim that 6.12's amdgpu "does not know
the card exists", based on no `1002:75xx` entries in its modalias list, was
wrong. The RX 7900 XTX (`1002:744c`) has no entry either and is plainly
supported, so that list is a legacy remnant, not the binding table. Both kernels
carry 9 `gc_12_0_0` (Navi 48) firmware references. The real blocker was the
missing firmware package, now fixed. 7.1.8 is still the better kernel for a new
card, but it was not the emergency described.

What cannot be done cleanly: AMD publishes ROCm only for `jammy` and `noble` —
there is no Debian tree (404). Debian's own ROCm is 6.1.2, which predates
gfx1201. Checking the noble 7.2.4 packages against trixie:

- `hip-runtime-amd`'s external deps (`libnuma1`, `libstdc++6`, `libgcc-s1`,
  `libc6`) are all satisfiable here;
- `libpython3.12` does not exist on trixie (3.13), breaking the Python-binding
  packages — the `amd-smi` CLI, rocprofiler;
- `libelf1` is `libelf1t64` on trixie.

Options:

- **Full ROCm 7.2.4 from AMD's noble repo, apt-pinned** so it can never upgrade
  a system library. Closest to "the works"; the Python-binding pieces are
  expected to fail, and the whole combination is unsupported by AMD.
- **Minimal host ROCm.** Debian-native `amd-smi` / `rocminfo` (6.1.2) purely to
  see and manage the cards; containers carry the real ROCm. Native packages, no
  mixing, but the tooling is too old to report gfx1201 properly.
- **Driver and firmware only.** Stop where we are. Containers bring their own
  complete ROCm, which is the standard pattern and the original plan.

## Fitting the GPUs will probably rename the 10G NIC and strand NovaNAS

NovaNAS is single-homed on `enp6s0` with a static address set by name in
`/etc/network/interfaces`, and there is no remote rescue path. That name is
derived from its PCI bus: the Aquantia 10G sits at `06:00.0`.

Today there is no `00:01.0` root port in `lspci` — the CPU's x16 complex is
dormant because nothing is plugged into it. Fitting two R9700s activates it, and
it claims bus numbers ahead of the PCH root ports that currently hold the NVMe
drives, the SATA controller and the NICs. Everything downstream shifts up, so
`06:00.0` most likely becomes `08:00.0` and the interface comes up as `enp8s0`.

`/etc/network/interfaces` matches on `enp6s0` literally. If the name changes the
stanza never applies: no address, no default route, nothing listening, and
recovery needs a monitor and keyboard.

Two other places also name the interface, and would break the same way:
`/etc/samba/smb.conf` (`interfaces = lo enp6s0`) and the kube-vip DaemonSet
(`vip_interface=enp6s0`), which owns the `192.168.10.211` buildkit VIP.

Options:

- **Pin the name to the MAC, keep `enp6s0`.** A systemd `.link` file matching
  `a8:b8:e0:14:20:d2` and forcing `Name=enp6s0`. Nothing else changes — smb.conf
  and kube-vip keep working untouched. systemd warns about assigning a name in
  its own predictable namespace, but it is widely done and stable.
- **Rename to something bus-independent, e.g. `lan10g`.** Cleaner in principle
  and unambiguous, but the name is referenced in three places, so
  `/etc/network/interfaces`, `smb.conf` and the kube-vip DaemonSet all have to
  change together.
- **Leave it and fix at the console.** The rename only bites at the next boot,
  which is the boot where the cards go in — and you will be physically at the
  machine anyway.

## Remaining host gaps for NovaNAS as a Kubernetes LLM box

Backports is exhausted: nothing installed has a newer backports candidate. The
three that mattered — kernel 7.1.8, zfs 2.4.4, firmware-amd-graphics 20260810 —
are in. The only remaining backports delta is smartmontools 7.4 -> 7.5.

The more notable finding is not a backports question. **intel-microcode is not
installed at all.** The CPU is an i5-14400T (Raptor Lake, family 6 model 191
stepping 2) running microcode 0x3d supplied by the BIOS; Debian omits the
package by default because it lives in non-free-firmware. For a host about to
run sustained inference, leaving the CPU on whatever revision the board shipped
is an avoidable risk. trixie has 3.20251111.1.

Smaller gaps, all plain trixie, all currently absent: `ethtool` (this is a 10G
host and its absence already obstructed NIC diagnosis earlier today),
`lm-sensors` (thermals, with two 300W-class cards arriving), `numactl` (vLLM
pinning).

Checked and deliberately not recommended: `libdrm2` / `libdrm-amdgpu1` — ROCm
bundles its own (`librocm_sysdeps_drm.so.2`), so the system copies are
redundant. ROCm's other runtime deps are already satisfied.

Separately, CDI is not enabled in k3s's containerd and no `/etc/cdi` spec dirs
exist. That is how AMD GPUs reach pods, but it belongs with the device-plugin
work after the cards are fitted.

Options:

- **Microcode plus the operational tools.** intel-microcode, ethtool,
  lm-sensors, numactl, and smartmontools from backports. Microcode needs a
  reboot to take effect, which the card installation provides anyway.
- **Microcode only.** The one with real consequences; skip the tooling.
- **Tools only, no microcode.** Avoids any change to CPU behaviour.
- **Nothing.** The box is functional as it stands.

## Bringing NovaNAS onto Cilium to match the kw cluster

NovaNAS currently runs flannel in `host-gw` mode (`cni0`, 10.42.0.0/24) with
kube-proxy in-process. kw runs **Cilium 1.19.4** installed by Helm (release
`cilium` in kube-system, revision 10), with cluster-pool IPAM over 10.42.0.0/16
at /24 per node, `routingMode: tunnel` / vxlan, `kubeProxyReplacement: true`,
BGP control plane on, and Hubble with relay, UI and metrics.

Three of kw's values cannot be carried over literally:

- `k8sServiceHost: 192.168.10.102` is kw's API server; on NovaNAS it must be
  192.168.10.203 or Cilium points at the wrong cluster.
- `hubble.metrics.serviceMonitor.enabled: true` needs the ServiceMonitor CRD,
  which NovaNAS does not have — Prometheus was removed earlier today — so the
  Helm install fails outright.
- `hubble.ui.ingress` targets ingress class `nginx` at `hubble.kw.watteel.lab`.
  NovaNAS has no ingress controller at all (traefik is disabled), so the Ingress
  object would be created and never served.

The substantive risk is `kubeProxyReplacement: true`. NovaNAS runs kube-vip in
ARP mode with service election, holding 192.168.10.211 for buildkit. Cilium
replacing kube-proxy is known to interfere with kube-vip's service VIP
handling, and that VIP is how CI reaches the builder.

The migration itself is disruptive whatever is decided: k3s must restart with
`--flannel-backend=none --disable-network-policy`, `cni0` and the flannel state
have to be torn down, and all 29 pods restart — ARC runners drop mid-job and
buildkit goes offline. SSH is unaffected, so the host stays reachable; a failed
Cilium rollout leaves the cluster without a CNI but recoverable.

Options:

- **Match kw closely, minus what cannot work.** Cilium 1.19.4, cluster-pool
  IPAM, vxlan tunnel, BGP on, Hubble with relay and UI, but
  `k8sServiceHost: 192.168.10.203`, serviceMonitor off, UI ingress off (reach it
  by port-forward). Keep `kubeProxyReplacement: true` as kw has it, and accept
  that kube-vip may need replacing with Cilium's own LB.
- **Same but keep kube-proxy.** Identical except `kubeProxyReplacement: false`,
  leaving kube-vip and the buildkit VIP untouched. Diverges from kw on one
  setting, and is the lower-risk path for a box whose CI depends on that VIP.
- **Match kw fully, including LB.** Drop kube-vip and let Cilium handle
  LoadBalancer addresses via its own IP pool, which is the more coherent
  end-state but changes how .211 is served.
- **Do not migrate.** Leave flannel in place.

**Decided:** migrated. NovaNAS now runs cilium 1.19.4 matching kw on chart,
IPAM, routing mode, kube-proxy replacement, hubble and BGP control plane.
kube-vip was NOT retired — investigation showed kw runs Cilium _with_ kube-vip
for LoadBalancer and has no CiliumLoadBalancerIPPool or L2 announcement policy,
so "full parity including LB" meant keeping kube-vip and bumping it to kw's
v1.2.3. k8sServiceHost, hubble serviceMonitor and hubble UI ingress necessarily
differ, for the reasons recorded above.

## Whether to change the shell used on the Mac

Several failures this session came from Mac-side inline commands written with
bash idioms but executed by zsh — most damagingly `K="kubectl --context=kw"; $K
get nodes`, which zsh reads as one command name because it does not word-split
unquoted expansions. Combined with `2>/dev/null` it failed silently, and the
empty output was misread as "kw has no kube-vip, no LB pools, no L2 policies".

Switching shells does not obviously help. zsh here is 5.9, current and capable.
macOS `/bin/bash` is 3.2.57, frozen at 2007 over GPLv3 licensing, lacking
`mapfile`, associative arrays and `${var,,}` — making it the tool shell would be
a regression. No Homebrew bash is installed, and Claude Code exposes no
shell-selection setting in this config. Every substantial piece of work this
session already ran as a script file through `ssh host 'sudo bash -s'` against
Linux bash 5.2 and behaved correctly.

Options:

- **Change nothing; fix the practice.** Non-trivial shell goes in a file with an
  explicit interpreter, inline commands avoid shell-specific constructs, and
  stderr is never suppressed on a check whose emptiness will be read as fact.
  Recorded as a memory so it persists.
- **Install Homebrew bash 5.x.** Leaves zsh as the login shell but makes a
  modern bash available at /opt/homebrew/bin/bash for scripts, lifting the 3.2
  limitations on locally-run scripts.
- **Change the login shell to bash.** Would make the tool and the interactive
  shell match, but on stock macOS that means bash 3.2, and it changes the
  interactive environment.

## The operator cannot adopt the deployments it is meant to manage

Three label schemes are in play, and no two agree:

| Where                       | Selector                                    |
| --------------------------- | ------------------------------------------- |
| `deploy/kubernetes/base/`   | `app.kubernetes.io/name` + `component`      |
| `operator/src/resources.rs` | those **plus `app.kubernetes.io/instance`** |
| the live cluster            | legacy `app: <name>`                        |

`spec.selector` is immutable on a Deployment, so the operator can adopt
neither. It has been failing to reconcile every 15 seconds for nine days:

```
spec.selector: Invalid value: {"app.kubernetes.io/..."}: field is immutable
spec.strategy.rollingUpdate: Forbidden: ... when strategy type is 'Recreate'
```

That failure is the only reason the cluster is healthy. The live Service and
PDB were made by an older operator build that used `app:`, they match the live
pods, and they work — Service has endpoints, PDB reads 2/2. Had the selector
been mutable, a successful reconcile would have rewritten **both** to labels
matching zero pods, taking the gateway Service down with the PDB. Issue #16
reports only the PDB, and reports the Service as working; it is working by
accident, and it is the more dangerous half.

The `instance` label is what makes the operator's selector un-adoptable, and it
is there for a reason: without it two `FastllmProxy` objects in one namespace
select each other's pods.

Options:

- **Drop `instance` from the operator's selector.** It then matches
  `deploy/kubernetes/base/` exactly and can adopt a standard install. Cost: two
  instances in one namespace would collide, which the label exists to prevent.
- **Add `instance` to the static manifests.** Keeps multi-instance isolation
  and makes a fresh install adoptable. Cost: the manifests must carry the CR's
  name, so they stop being name-agnostic.
- **Have the operator read the existing Deployment's selector** and use it for
  the Service, PDB and topology spread rather than assuming. Correct in every
  case including adoption, and the only one that fixes a cluster already on the
  legacy scheme without recreating anything. More code.
- **Stop the operator managing pod-selecting objects it did not create.**
  Narrowest fix for #16 alone; leaves the reconcile loop failing.

Whatever is chosen, the live cluster's Deployments are on the legacy `app:`
scheme and cannot be relabelled in place — moving them needs a delete and
recreate of both Deployments, which is a brief gateway outage.

**Decided:** backfilled `qwen3.5-9b` from its engine, then made it
self-maintaining — the health probe reads the window out of the `/models`
response it already fetches, and the registry prefers that over the stored
value. The backfill still matters as the fallback for anything not yet probed.

## A stale scratch worktree blocks every commit

`web/` is upgraded — vite 8.3.0, nanoid 3.3.19, `npm audit` clean, build and
both harnesses passing. The commit gate still refuses on vite 5.4.21 and
nanoid 3.3.17, and the only file left in the tree claiming those versions is:

```
.kilo/worktrees/verbose-manx/web/package-lock.json
```

a 15 MB scratch worktree another tool left behind. It is untracked, already in
`.git/info/exclude`, and I added `.kilo/` to `.gitignore` — none of which helps,
because the gate walks the filesystem rather than git. So nothing can be
committed until that copy stops claiming a vulnerable version.

Options:

- **Delete `.kilo/worktrees/verbose-manx/`.** Unblocks immediately. It is
  scratch and untracked, but it is a worktree and deleting it is irreversible
  — if it holds work nobody pushed, that work is gone.
- **Upgrade the dependencies inside it.** Non-destructive and unblocks, but it
  means running `npm install` in someone else's worktree and rewriting their
  lockfile.
- **Exclude it from the gate.** Correct in principle — a scratch copy is not
  this repository — and needs whatever ignore mechanism procoder itself reads,
  which `.gitignore` is not.

**Decided:** pending.

## Cleaning up the Kubernetes objects that used the tank ZFS pool

tank is held un-imported while its four raidz members are unplugged, pending
right-angled cables. Six k8s objects reference it: StorageClasses
`openebs-zfspv` (marked default) and `openebs-zfspv-block`, PVC
`buildkit/buildkit-cache` (150Gi), its PV `pvc-d3a6c144-...`, the ZFSVolume CR
of the same name, and the Deployment `buildkit/buildkit` that mounts it -- plus
the openebs zfs-localpv driver itself.

The PV is `reclaimPolicy: Delete`. Deleting the PVC therefore does not merely
detach storage: openebs will destroy the dataset `tank/pvc-d3a6c144-...` once
the pool is importable again, taking the 9.38G buildkit cache with it. That is
rebuildable, but the deletion happens later and silently.

Two incidental findings: `local-path` and `openebs-zfspv` are BOTH marked as
the default StorageClass, which is a misconfiguration; and
`default/test-fs-snap` is a 145-day-old VolumeSnapshot on class
`novanas-snapshots` whose driver `csi.novanas.io` was never deployed and whose
source is gone -- an orphan independent of tank.

Options:

- **Stop the churn, keep the storage.** Scale buildkit to 0 so the mount
  retries and CSI errors stop, and leave PVC, PV, ZFSVolume and the
  StorageClasses untouched so everything returns by itself when the pool is
  imported. Also delete the orphaned test snapshot, which is dead regardless.
- **Remove the k8s objects but preserve the data.** Patch the PV to
  `reclaimPolicy: Retain` first, then remove the Deployment, PVC and PV. The
  dataset survives on tank and can be re-adopted later, but re-adoption is
  manual work.
- **Full removal including the dataset.** Delete the Deployment, PVC, PV and
  ZFSVolume with the reclaim policy left at Delete, drop both zfspv
  StorageClasses and uninstall openebs zfs-localpv. The buildkit cache dataset
  is destroyed when the pool returns, and buildkit must be reprovisioned from
  scratch.

## Closing the R9700 vLLM performance gap

Our first vLLM run on the two R9700s served Qwen3.6-35B-A3B-FP8 at 7.2 tok/s
single-stream, 45.8 tok/s aggregate over 8 streams, with both cards at 100% and
31.9GB VRAM each, so sharding is genuinely working.

github.com/andysalerno/r9700-serving benchmarks the same hardware and reaches
57.7 tok/s single-stream (Qwen3.8-27B-FP8, 2026-09-11) and 105.1 tok/s
(Qwen3.6-27B-FP8, 2026-07-22). Both are _dense_ 27B models; ours is an MoE with
3B active parameters and should decode faster, so the gap is stack, not model.

Their stack differs in: ROCm 10.0 vs our 7.14.1; vLLM 0.29.0 compiled for
gfx1201 vs our stock 0.23.1.dev1 image; AITER compiled for gfx1201 (ours fell
back to Triton); Flash Attention 2.8.4 built for gfx1201; attention backend
ROCM_AITER_UNIFIED_ATTN vs our ROCM_ATTN; MTP speculative decoding with 3 tokens
vs none; fp8 KV cache vs 16-bit; and the env overrides GPU_MAX_HW_QUEUES=1,
NCCL_P2P_DISABLE=1, NCCL_MIN_NCHANNELS=112.

Separately, vLLM warns there is no tuned FP8 block-GEMM config for the R9700.
Upstream ships 219 such configs and the only AMD entries are MI300X/MI325X/
MI325_OAM, so no RDNA config exists to download; generating one needs vLLM's
kernel tuning script run on this hardware.

Their numbers are self-reported, and their own July result exceeds their
September one, so treat the targets as indicative.

Options:

- **Cheap wins on the existing image first.** Restart the current container with
  `--kv-cache-dtype fp8`, the three env overrides, and MTP speculative decoding
  if the model supports it. No rebuild; minutes, not hours. Establishes how much
  is configuration rather than compilation before committing to a build.
- **Reproduce their full build.** Use their Dockerfile.fullbuild pins: ROCm 10,
  vLLM 0.29.0, PyTorch 2.13, AITER and Flash Attention compiled for gfx1201.
  This is where the AITER win lives, but it is a long compile and a second large
  image alongside the 74.6GB one already pulled.
- **Tune the FP8 kernels for the R9700.** Run vLLM's tuning script to generate
  the missing config. Complements either path and nobody upstream has done it.
- **Stop here.** The cards are proven working and sharding correctly; treat
  performance as a later project.

## Models that advertise no context window because nobody filled the field in

`ed3aed2` makes `/v1/models` report `max_model_len` / `context_length`, taken
from `provider_models.context_length`. Four names still advertise nothing,
because that column is null for them:

| name               | engine says | column says |
| ------------------ | ----------- | ----------- |
| `qwen3.5-9b`       | 262144      | null        |
| `gpt-5`            | —           | null        |
| `gemini-2.5-flash` | —           | null        |
| `free`             | —           | null        |

`qwen3.5-9b` is the sharp case: the engine on `.245:8001` publishes 262144 in
its own `/v1/models`, and a client asking the proxy is told nothing. The data
exists, it simply never reached the column.

Options:

- **Backfill the column for the local models.** One `UPDATE` per model from
  what each engine reports. Immediate, and the numbers are authoritative
  because they come from the process actually serving. Goes stale if a model
  is restarted with a different `--max-model-len`.
- **Have the proxy read it from the backend instead.** The engine scrape
  already runs per backend; `/v1/models` is one more cheap call. Self-healing
  and never stale, but it is a new thing on the scrape path and only works for
  engines that publish it — the cloud models still say nothing.
- **Have the registration agent record it.** The agent already registers what
  each host serves, so it could carry the context length with it. Right place
  for it long-term; does nothing for cloud providers or hand-registered
  endpoints.
- **Leave it.** Omitted is honest. The two models that matter for coding
  traffic already advertise 262144.

**Decided:** pending.

## Making the vLLM service on NovaNAS survive restarts

The MXFP4 model is serving on port 8000 from a container started by hand
(`docker run`), with tool calling, the qwen3 reasoning parser, MTP speculative
decoding, fp8 KV cache and a 262144 context. It holds both GPUs, so anything
else wanting them fails while it runs.

As it stands it does not survive a reboot, and it has no supervision. A
`--restart unless-stopped` policy was tried and removed: combined with a corrupt
torch.compile cache it produced a crash loop, each restart retrying the same bad
cache. The cache has since been cleared and the container is healthy, but the
episode is the argument for supervision that notices a failing start rather than
repeating it blindly.

Options:

- **Leave it as it is.** A hand-started container, fine for evaluation. Gone
  after a reboot, and nothing restarts it if it dies.
- **Add a Docker restart policy.** One flag, `--restart unless-stopped`. Comes
  back after a reboot, but retries indefinitely on a poisoned compile cache,
  which is exactly the loop just escaped.
- **Write a systemd unit.** Proper supervision with start limits so a repeated
  early failure stops rather than loops, ordered after docker.service, and
  visible in `systemctl --failed` alongside everything else on that host.
- **Move it into k3s.** Consistent with how the rest of the box is run, but the
  GPUs are not exposed to Kubernetes yet -- that needs the AMD device plugin and
  CDI, which was deliberately deferred until after the cards were fitted.

**Decided:** leave it as a hand-started container. It is an evaluation setup,
not a service; it will not come back after a reboot and nothing supervises it.

## Deploying the 1M-context Qwen3.8-27B MXFP4 model at max context

`Solstice-AI/Qwen3.8-27B-TURBO-...-MXFP4-1M` is a dense 27B in MXFP4
compressed-tensors, 18.9GB of weights, with `max_position_embeddings` of
1,048,576 reached via YaRN (factor 4.0 over a 262144 native base).

Its KV cost is an order of magnitude worse than the MoE currently deployed. Of
64 layers, 16 are `full_attention` (the rest linear), with 4 KV heads at
head_dim 256 -- **64 KiB per token at bf16**, against 6.3 KiB for the MoE.

Holding a full 1M-token context therefore needs a 64 GiB KV pool at bf16. That
exceeds the 64GB of VRAM on the pair before the 18.9GB of weights are counted,
so it cannot be done. At fp8 the same context needs ~32 GiB, which does fit
alongside ~9.5GB of weights per card.

So the model's headline 1M context is reachable only with
`--kv-cache-dtype fp8` -- the flag just removed at the user's request while
investigating garbled reasoning output. That garbage was never reproduced and
fp8 KV was never shown to cause it, but it has not been cleared either.

Deploying also requires the GPUs, which the current model holds. Its container
can be stopped without removing it; weights, configuration and warm compile
cache all survive.

Options:

- **1M context with fp8 KV.** The advertised maximum, but re-enables the flag
  just removed. If the garbled output returns, fp8 KV becomes the prime suspect
  again -- which is itself information.
- **~650K context, no fp8 KV.** The largest that fits with an unquantised KV
  cache, keeping the change that was just made. Still far beyond the ~18K the
  client actually sends.
- **262144 context, no fp8 KV.** The model's native pre-YaRN length, avoiding
  rope scaling entirely and leaving generous KV headroom for concurrency.

## Whether to keep DFlash2 speculative decoding on the 27B target

The 27B target is `Solstice-AI/Qwen3.8-27B-TURBO-...-Heretic-Uncensored-NM-DAU-MXFP4-1M`,
a heavily merged and abliterated derivative of `Qwen/Qwen3.8-27B`, served at
524288 context with fp8 KV over TP=2.

`z-lab/Qwen3.8-27B-DFlash2` loads and runs -- vLLM accepts it as
`method='dflash'`, and it is structurally correct: hidden_size 5120 and vocab
248320 both match the target, and its `model_type: qwen3` avoids the trap that
blocked a standalone 4B (any `qwen3_5` draft is forcibly reclassified as MTP and
then fails on 5120 vs 2560).

It is nonetheless a net loss. Draft acceptance is 5.0% (139 of 2768 tokens), so
the drafting cost is paid every step and almost never avoids a target forward
pass. Throughput falls from 39.0 to 28.5 tok/s, and the draft's 3.85GB takes the
KV pool from 17.03 to 12.66 GiB, cutting concurrency at full context from 2.09
to 1.49. The cause is semantic, not structural: DFlash2 is trained against stock
Qwen3.8-27B, and this merge has drifted too far for its predictions to match.
Output quality is unaffected either way, since the target verifies every drafted
token.

Options:

- **Remove the draft.** Back to 39.0 tok/s and the full 17.03 GiB KV pool.
  Speculative decoding is simply not available for this target.
- **Keep it.** No quality risk, but 27% slower and less concurrency headroom --
  no reason beyond leaving the plumbing in place for a future draft.
- **Lower num_speculative_tokens to 1 or 2.** Reduces the wasted drafting per
  step; at 5% acceptance it is unlikely to turn positive, but it is cheap to
  measure before giving up on speculation entirely.

## What to follow up after the Kuvryn-Scout latency diagnosis

The latency question is answered: fastllm-proxy contributes ~13ms (p50 over
28,972 embedding calls on the same path), and the time is in the engines.
Thinking is genuinely off where the UI says Off -- `reasoning_effort:"none"` is
sent on the wire and honoured by the 9b (0.31s / 0 thinking tokens, versus
14.58s / 300 thinking tokens with the field absent). Context is sub-second; the
16s figure was Context and Adjudication averaged together under one model name.

Two unresolved items came out of the investigation, neither of which was the
original question.

Options:

- **Investigate the 502s.** 108 of the 9b's calls failed in two days, roughly
  12% of its traffic, against 14 on the 35B and 8 on the 27b. These are
  failures rather than slowness and are concentrated on one engine, so the
  cause is probably local to `:8001` -- KV exhaustion under its 0.17 memory
  share, or the engine dropping requests when the shared GB10 saturates.
- **Retune the 9b engine.** It runs on vLLM defaults: no speculative decoding,
  no flashinfer attention, no fp8 KV cache, no chunked prefill, while the 35B
  beside it has all four plus MTP. Measured engine-direct, the 9b does 15.6
  tok/s against the 35B's 34-40 -- slower than a model four times its size.
  This only pays off for the roles that keep thinking on (Deep review,
  Verification, Adjudication). It means restarting a model others are using.
- **Both**, 502s first, since a failing call costs more than a slow one.
- **Neither.** The original question is answered; leave the rest.

**Decided (2026-09-22): neither.** The latency question was the ask and it is
answered. The 502s on `:8001` and the 9b's untuned engine flags are both real
and both still open -- recorded here so they are not rediscovered from scratch.

## How to apply the better 9b deployment config

Follow-on from the Kuvryn-Scout latency work. The 9b was **not** started by hand:
it is kuvryn-managed (`ai.kuvryn.managed=true`, deployment
`6468dd59-e787-4796-9d4d-75ceeb0c75bc`, generation 2), with desired state pulled
from `https://kuvryn-ai.kw.watteel.lab`. Editing the container locally would be
reverted on the agent's next poll, so the change belongs in the kuvryn
deployment config. `port` is a top-level field separate from `config`, so none
of this touches 8001.

The binding constraint is not the flags I first guessed. From the engine's own
startup log: KV cache 9.7 GiB / 313,738 tokens, and **"Maximum concurrency for
262,144 tokens per request: 1.20x"**. It also crashed once on first boot
(`max_restarts: 1`) because 262144 needed 8.1 GiB of KV against 7.65 GiB
available. Meanwhile the largest prompt ever sent to it in 7 days is 35,067
tokens (p99 28,611). The 262144 context is ~7x more than anything used and is
what holds concurrency at 1.2 requests.

Two earlier claims of mine were wrong and are corrected here: the model is
NVFP4-quantized, not bf16, so weights are ~5GB and KV was never starved by
weight size; and it is a `qwen3_5` multimodal architecture with no MTP layers,
so the 35B's `speculative_config` cannot be copied to it (a `qwen3_5` draft is
forcibly reclassified as MTP and fails -- the same trap recorded above for the
27B).

Proposed config change (all keys verified against `internal/api/advanced_config.go`):

    max_model_len          262144 -> 65536      concurrency 1.20x -> ~4.8x
    kv_cache_dtype         (unset) -> "fp8"     roughly doubles KV -> ~9.5x
    attention_backend      (unset) -> "flashinfer"
    enable_chunked_prefill (unset) -> true      smooths the 9k-35k prefills
    async_scheduling       (unset) -> true
    max_num_batched_tokens (unset) -> 16384
    load_format            (unset) -> "fastsafetensors"   (startup only)

`gpu_memory_utilization` stays at 0.17 so the shared GB10 reservation is
undisturbed. This likely also addresses the 108 502s: at 1.2x concurrency the
queue backs up and the proxy's headers timeout fires.

I cannot apply it -- `/api/v1/deployments` returns 401 and there is no kuvryn
credential on this machine or CLI installed.

Options:

- **Pascal applies it in the kuvryn UI** from the JSON above.
- **Pascal supplies an API token** and I apply it, then verify with the same
  engine-direct benchmark before and after.
- **Only `max_model_len`**, as the single lowest-risk change that does most of
  the work; leave the flags for later.
- **Leave it.** The deployment keeps its current behaviour.

**Decided (2026-09-22): leave it as is.** No config change, no restart. The
analysis above stands on its own if this is revisited -- the measured
concurrency ceiling, the prompt-size evidence for 65536, and the two corrected
claims about the model.

## What to pick up after the OpenRouter provider-routing answer

Two threads are open at once and neither is finished.

**GLM-5.3-Flash / Z.ai.** The provider and the key are both fine: the key Pascal
supplied returns 200 on `/models` at both `api/coding/paas/v4` and
`api/paas/v4`, the catalogue lists `glm-5.3-flash`, and Z.ai is
case-insensitive, so `GLM-5.3-Flash` completes successfully. The original 401
is gone since Pascal set the key himself. What blocks it now is DNS, not
credentials: `api.z.ai` gets **SERVFAIL from 192.168.10.136** while
**192.168.10.139 answers it normally**, and .136 resolves it once DNSSEC
checking is disabled (`dig +cd`). So .136 is failing DNSSEC validation on the
`api.z.ai` -> Alibaba Global Accelerator (`*.aliyunga0017.com`) CNAME chain. CoreDNS
forwards to the node's resolv.conf, so the cluster inherits the failure and the
health probe reports "Temporary failure in name resolution". Because the probe
never reaches Z.ai, the key Pascal set is still unverified. Pascal also asked me
to set the token; I could not -- `/admin/*` needs a session cookie and there is
no admin password or kuvryn-style credential available in this session.

**OpenRouter backend pinning.** Answered: no change needed for the per-request
case. A client can send OpenRouter's `provider` block today -- the SDK merges it
via `providerutil.ApplyProviderOptions` (`maps.Copy` into the top level) and
FastLLM relays it, which I proved with a probe test on kw covering both the
byte-identical path and the alias re-serialising path. What does _not_ exist is
configuring it per backend so every request carries it.

Options:

- **Fix the DNS validation on 192.168.10.136** so `api.z.ai` resolves, then
  confirm the Z.ai probe clears and GLM-5.3-Flash actually serves. This is the
  blocker for the model Pascal just made.
- **Point CoreDNS at 192.168.10.139** (or add an override for api.z.ai) as a
  narrower change that unblocks the cluster without touching .136.
- **Add per-backend extra body fields to FastLLM**, so an OpenRouter backend can
  pin `provider.order` without every caller sending it. A new column plus a
  merge in `rewrite_model_if_needed`.
- **Neither** -- both threads are documented well enough to resume later.

**Decided (2026-09-22): neither.** Stopping here. The OpenRouter question is
answered and needs no code. The Z.ai DNSSEC finding and the unverified token
are recorded above so they can be resumed without re-deriving anything.

## Scope of the Cloudflare Tunnel exposure for FastLLM

The cluster already runs the pattern: `cloudflared-named` in the `novagrade`
namespace, `cloudflare/cloudflared:latest`, args `tunnel --no-autoupdate run`,
`TUNNEL_TOKEN` from secret `cloudflared-token`, 2 replicas, routing held in the
Cloudflare dashboard rather than a local config file. Nothing about FastLLM
makes it a harder case; there is no ingress in the `fastllm` namespace today and
everything is reached over kube-vip LoadBalancer VIPs on the LAN.

The constraint that shapes the design is Cloudflare's 100-second origin timeout
(524), which measures time to first byte. Measured over 7 days of real traffic:
non-streaming 92,593 calls, 1,012 of them over 100s (1.09%); streaming 12,277
calls with TTFB averaging 9.7s and only 68 over 100s (0.55%). So roughly 1% of
present traffic would fail, concentrated in the deep-review role that averages
98s. The limit is Enterprise-only to raise, so the mitigation is to stream the
long-running roles rather than to configure anything.

Also noted while measuring: non-streaming max duration 2,085s and streaming max
11,405s, which look like stuck connections and deserve their own look.

Security shape: the gateway's API-key auth is the real boundary and is sound
(SHA-256 hashing; `authorization` is in `REQUEST_ONLY_STRIPPED` so a client key
never reaches a provider). The control plane on :4001 is a password-only admin
UI. `fastllm-pg-dev` is a LoadBalancer on 192.168.10.127 and must never be
routed. Every client header other than host/content-length/authorization and
hop-by-hop is forwarded upstream, which is worth an allowlist once anonymous
callers can reach it.

Options:

- **Gateway only.** One hostname to `fastllm-proxy:4000`. Admin stays LAN-only.
  Smallest surface; API keys plus per-principal budgets and Cloudflare rate
  limiting carry the security.
- **Gateway, plus the control plane behind Cloudflare Access.** Adds remote
  admin with SSO in front of the existing password.
- **Gateway behind Cloudflare Access service tokens too.** Tightest, but every
  client must then send Access headers as well as its API key, which breaks
  plain OpenAI-compatible SDK usage.
- **Discuss only for now**, build nothing.

**Decided (2026-09-25): gateway only.** One hostname to `fastllm-proxy:4000`.
The control plane and `fastllm-pg-dev` stay LAN-only. Security rests on the
existing API-key auth plus per-principal budgets, with Cloudflare rate-limiting
as a second layer. Streaming is the mitigation for the 100s edge timeout.

## A live `coder` outage, a dropped LB member, and why the cache never stores

Three findings from the cache question, in order of urgency.

**1. `coder` is failing ~95% of requests against a working engine.** The proxy
has `192.168.10.245:8000` marked `fastllm_backend_healthy 0` and is answering 502. The engine is fine: it returns `/v1/models` in 13ms and completed a real
generation in 96ms while marked unhealthy, its container has been up 8 days,
and the control plane's own probe has it un-degraded with `last_seen_at` a few
seconds old. Failure rate by hour: 0% through 04:00, 63.9% at 05:00, 96.0% at
06:00 (1,402 errors against 58 successes). Backend health lives in the proxy's
memory, so the two pods disagree with the control plane and with reality.
`sweep()` calls `mark_probe_ok()`, which does restore health, and no
"back in rotation" or "out of rotation" line appears in 20 minutes of logs --
consistent with an ejection that predates the window and a probe that is not
clearing it.

**2. Load balancing has nothing to balance.** `192.168.10.246` was down: its
engine answered nothing on 8000/8001/8890/8891, and its `model_backends` row
for the 35B was gone, leaving `qwen3-6-35b-a3b-nvfp4` with exactly one backend.
It came back during the investigation -- containers 1-2 minutes old, `:8000`
answering 200, `providers.last_seen_at` now populating -- but it still has no
`model_backends` row, so it is a known provider attached to no model.

**3. The cache cannot store anything given the current traffic.** Not a
key-matching problem: `store` is 0 across all replicas while `miss` is ~1,800,
so nothing is ever written. Only non-streaming 2xx responses are cacheable, and
right now every non-streaming request to that model is a 502 (errors are
deliberately never cached) while every success streams (streams are
deliberately never cached). Separately, the proxies log "snapshot changed,
registry rebuilt" every 10-30s while the control plane rebuilt once in six
minutes, and `apply_snapshot` clears the cache -- so even once stores resume,
entries would be wiped far more often than the 300s TTL implies. That mismatch
looks like a bug in the proxy's change detection and is worth its own look.

Options:

- **Restart the two proxy pods.** Backend health is in-memory, so this clears
  the false ejection and should restore `coder` immediately. Cheapest fix for
  the outage, but it treats the symptom and loses the evidence.
- **Re-attach `.246` as a backend** so there are two members to balance across,
  now that the box is serving again.
- **Investigate why the probe is not restoring health** before restarting,
  while the bad state is still there to inspect.
- **Investigate the snapshot-churn mismatch** (proxy rebuilding 10-30x more
  often than the control plane rebuilds).

## `/metrics` is the operator's liveness probe, so gating it crash-loops the proxy

Deploying the `/metrics` auth gate (`1d3936f`) to kw put the new pod into a
restart loop: `Startup probe failed: HTTP probe failed with statuscode: 401`,
five restarts before I rolled the CR back to `sha-169bfd9`. Both old pods stayed
ready throughout, so there was no outage, but `main` currently carries a change
that will do this to anyone who deploys it.

The cause is a deliberate design I did not account for.
`operator/src/resources.rs:542-544` probes `/health` for readiness but
`/metrics` for liveness and startup, because `/health` answers 503 when no
backend is healthy. That is correct for readiness -- stop sending traffic -- and
would be a disaster for liveness, where it would restart every proxy pod during
a backend outage. So the operator needed an endpoint that is 200 whenever the
process is alive, and `/metrics` was the only one.

Gating `/metrics` took that away. The three ways out:

Options:

- **Add a `/livez` endpoint** -- always 200 while the process lives, no auth, no
  body -- and point the operator's liveness and startup probes at it. Correct
  separation, and it is what the operator actually wanted. Costs a three-step
  rollout: the proxy must serve `/livez` before the operator probes it, and
  `/metrics` can only be gated once no probe depends on it.
- **Serve an empty exposition to an unauthenticated scrape** -- 200, one comment
  line saying a key is required, no series. Probes pass unchanged, nothing is
  disclosed, no operator change and no sequencing. The compromise is answering
  200 to a request that was not authorised, and a misconfigured scraper sees an
  empty target rather than a clear 401.
- **Revert the `/metrics` half** and gate only `/health`, leaving the 62KB
  exposition public. Smallest change; gives up most of the disclosure fix, since
  `/metrics` was the bigger leak of the two.

## Which name the dashboard should answer on

Pascal asked for `fastllm.kw.watteel.dev`, "similar like how other apps give me
access". Those two are not the same thing here, so it is worth settling before
building.

Every app he is comparing against is on **`.lab`**: `kuvryn-ai`, `kuvryn`,
`scout`, `grafana`, `nexora`, `headlamp`, `hubble`, `dhole`, `novamem` — all
`<name>.kw.watteel.lab`, all resolving to `192.168.10.120`, the nginx ingress,
each with a cert-manager `Certificate` off the `cluster-ca` ClusterIssuer. That
is a LAN-only pattern; the CA is internal.

**`.dev` is a different thing.** `fastllm.kw.watteel.dev` already resolves —
to Cloudflare (104.21.18.206 / 172.67.183.88) — but nothing serves it: both it
and `nexora.kw.watteel.dev` answer with no connection at all. So `*.kw.watteel.dev`
is wildcard DNS at Cloudflare with no origin behind it, and putting the
dashboard there means publishing it on the public internet, not adding an
internal name.

That matters more than usual because this is the **admin dashboard**, not the
gateway. The session already decided (see the tunnel scope above) that the
control plane is a password-only admin UI and should not be internet-facing
without Cloudflare Access in front.

Mechanics are the same either way and are already understood: the control plane
serves TLS itself on 4001 (plain HTTP there gets nothing), so the ingress needs
`nginx.ingress.kubernetes.io/backend-protocol: "HTTPS"` — which none of the
existing app ingresses need, since their backends are plain HTTP.

Options:

- **`fastllm.kw.watteel.lab`** — exactly the pattern every other app uses.
  Ingress on the nginx class, a `Certificate` from `cluster-ca`, LAN only.
  Nothing new is exposed; it just stops being an IP and a port.
- **`fastllm.kw.watteel.dev`, public** — a route on the existing Cloudflare
  tunnel to `fastllm-control:4001`. Reachable from anywhere, so it wants
  Cloudflare Access in front of the login rather than the password alone.
- **Both** — `.lab` for daily use, `.dev` behind Access for when away.
