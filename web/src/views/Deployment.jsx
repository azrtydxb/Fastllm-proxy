import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Banner,
  Button,
  Card,
  Dot,
  ErrorNote,
  Field,
  Grid,
  Loading,
  Mono,
  Muted,
  Pill,
  Row,
  Spacer,
  Stack,
  usePoll,
} from "../ui.jsx";

// The shape of the deployment, for the deployments that have one.
//
// Everything else in this UI edits a row in Postgres and takes effect on the
// next snapshot. This screen edits a `FastllmProxy` resource, and nothing here
// takes effect until the operator has rolled it out — which is a different
// promise, so the page says so rather than reporting "saved" and looking
// finished while two Deployments are still turning over.
//
// It is absent entirely without an operator: the shell only mounts it when
// `GET /admin/config` says `operator_managed`, and the routes behind it 404
// otherwise. A screen offering to scale a deployment that nothing reconciles
// would be a button that does nothing, which is worse than no button.

const POLL_MS = 5000;

const POLICIES = [
  ["cacheAffinity", "cache affinity — a shared prefix returns to the node holding its KV cache"],
  ["leastLoaded", "least loaded — cache-blind, for traffic with no prefix sharing"],
  ["roundRobin", "round robin — a baseline, cache-blind"],
  ["lowestLatency", "lowest latency — for a pool whose members are not equivalent"],
];

const PHASE_TONE = {
  Ready: "ok",
  Upgrading: "warn",
  Bootstrapping: "warn",
  Pending: "warn",
  Degraded: "bad",
};

/** A number input that reports "" as undefined, so an untouched field is not a change. */
function numeric(value) {
  if (value === "" || value === null || value === undefined) return undefined;
  const n = Number(value);
  return Number.isFinite(n) ? n : undefined;
}

export function Deployment({ onUnauthorised }) {
  const [draft, setDraft] = useState({});
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState(null);
  const { data, error, loading, reload, setError } = useLoader(
    () => api.get("/admin/deployment"),
    { onUnauthorised },
  );
  // Polled, because everything on this page is asynchronous by nature: the
  // answer to "did my change land" arrives from the operator, not from the
  // response to the PATCH.
  usePoll(reload, POLL_MS);

  if (loading && !data) return <Loading />;
  if (!data) {
    return (
      <Stack>
        <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>
      </Stack>
    );
  }

  const proxy = data.proxy || {};
  const status = data.status || {};
  const autoscaling = proxy.autoscaling || {};
  const field = (key, fallback) => (key in draft ? draft[key] : fallback ?? "");
  const set = (key) => (e) => {
    const v = e.target.type === "checkbox" ? e.target.checked : e.target.value;
    setDraft((d) => ({ ...d, [key]: v }));
  };
  const dirty = Object.keys(draft).length > 0;

  const save = async () => {
    // Only what was actually touched. A patch carrying every field would
    // rewrite values somebody may be managing in Git, and would bump the
    // resource's generation — a rollout — for fields nobody changed.
    const body = {};
    if ("image" in draft && draft.image.trim()) body.image = draft.image.trim();
    if ("replicas" in draft) body.replicas = numeric(draft.replicas);
    if ("policy" in draft) body.policy = draft.policy;
    if ("upstream_timeout" in draft) body.upstream_timeout = numeric(draft.upstream_timeout);
    if ("workers" in draft) body.workers = numeric(draft.workers);
    if ("pool_max_idle" in draft) body.pool_max_idle = numeric(draft.pool_max_idle);
    if ("autoscaling_enabled" in draft) body.autoscaling_enabled = draft.autoscaling_enabled;
    if ("autoscaling_min_replicas" in draft)
      body.autoscaling_min_replicas = numeric(draft.autoscaling_min_replicas);
    if ("autoscaling_max_replicas" in draft)
      body.autoscaling_max_replicas = numeric(draft.autoscaling_max_replicas);
    if ("autoscaling_target_cpu" in draft)
      body.autoscaling_target_cpu = numeric(draft.autoscaling_target_cpu);
    for (const k of Object.keys(body)) if (body[k] === undefined) delete body[k];
    if (Object.keys(body).length === 0) return;

    setBusy(true);
    const ok = await attempt(() => api.patch("/admin/deployment", body), setError, onUnauthorised);
    setBusy(false);
    if (ok) {
      setDraft({});
      setNote(
        "Applied to the FastllmProxy. The operator rolls it out — watch the phase above; " +
          "an image change rolls the control plane first and holds the gateway until it is ready.",
      );
      reload();
    }
  };

  const conditions = status.conditions || [];
  const notReady = conditions.find((c) => c.type === "Ready" && c.status !== "True");
  const upgrading = conditions.find((c) => c.type === "Upgrading" && c.status === "True");

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>
      {note && (
        <Card tone="accent">
          <Row>
            <Muted>{note}</Muted>
            <Spacer />
            <Button variant="small" onClick={() => setNote(null)}>
              dismiss
            </Button>
          </Row>
        </Card>
      )}

      {upgrading && <Banner tone="warn">{upgrading.message}</Banner>}
      {!upgrading && notReady && <Banner tone="warn">{notReady.message}</Banner>}

      <Card
        title={`FastllmProxy ${data.namespace}/${data.name}`}
        subtitle="the desired state an operator reconciles — changes here are a rollout, not a snapshot"
        right={
          <Pill tone={PHASE_TONE[status.phase] || "neutral"}>{status.phase || "unknown"}</Pill>
        }
      >
        <Grid cols={4}>
          <Stat label="SERVING" value={<Mono>{status.observedImage || "—"}</Mono>} />
          <Stat label="GATEWAY" value={status.proxyReplicas || "—"} />
          <Stat
            label="CONTROL"
            value={
              <Row gap={6}>
                <Dot tone={status.controlReady ? "ok" : "bad"} size={7} />
                {status.controlReady ? "ready" : "not ready"}
              </Row>
            }
          />
          <Stat label="CONFIG HASH" value={<Mono>{status.configHash || "—"}</Mono>} />
        </Grid>
      </Card>

      <Card
        title="Shape"
        subtitle="image, scale and routing policy. Secret references, service type and the admin bootstrap are deliberately not editable here — see operator/README.md"
      >
        <Grid cols={2}>
          <Field
            label="IMAGE"
            hint="both planes run it; the control plane rolls first and the gateway is held until it is ready"
          >
            <input
              value={field("image", data.image)}
              onChange={set("image")}
              spellCheck={false}
              style={{ fontFamily: "var(--mono)" }}
            />
          </Field>
          <Field label="POLICY" hint="backend selection for the gateway">
            <select value={field("policy", proxy.policy)} onChange={set("policy")}>
              {POLICIES.map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </Field>
          <Field
            label="GATEWAY REPLICAS"
            hint={
              autoscaling.enabled
                ? "left to the autoscaler while autoscaling is on"
                : "scales the data plane; the control plane stays at one"
            }
          >
            <input
              type="number"
              min="1"
              disabled={autoscaling.enabled}
              value={field("replicas", proxy.replicas)}
              onChange={set("replicas")}
            />
          </Field>
          <Field label="UPSTREAM HEADER TIMEOUT (s)" hint="bounds time to first byte, not generation">
            <input
              type="number"
              min="1"
              value={field("upstream_timeout", proxy.upstream_timeout)}
              onChange={set("upstream_timeout")}
            />
          </Field>
          <Field label="WORKERS" hint="empty means one per core">
            <input
              type="number"
              min="1"
              value={field("workers", proxy.workers)}
              onChange={set("workers")}
            />
          </Field>
          <Field label="IDLE UPSTREAM CONNECTIONS PER BACKEND">
            <input
              type="number"
              min="1"
              value={field("pool_max_idle", proxy.pool_max_idle)}
              onChange={set("pool_max_idle")}
            />
          </Field>
        </Grid>
      </Card>

      <Card
        title="Autoscaling"
        subtitle="an HPA on CPU. While it is on the operator stops writing the replica count, so the two never fight over it"
      >
        <Row gap={14}>
          <label style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <input
              type="checkbox"
              checked={!!field("autoscaling_enabled", autoscaling.enabled)}
              onChange={set("autoscaling_enabled")}
            />
            <Muted>enabled</Muted>
          </label>
        </Row>
        <Grid cols={3} style={{ marginTop: 12 }}>
          <Field label="MIN REPLICAS">
            <input
              type="number"
              min="1"
              value={field("autoscaling_min_replicas", autoscaling.minReplicas)}
              onChange={set("autoscaling_min_replicas")}
            />
          </Field>
          <Field label="MAX REPLICAS">
            <input
              type="number"
              min="1"
              value={field("autoscaling_max_replicas", autoscaling.maxReplicas)}
              onChange={set("autoscaling_max_replicas")}
            />
          </Field>
          <Field label="TARGET CPU %">
            <input
              type="number"
              min="1"
              max="100"
              value={field(
                "autoscaling_target_cpu",
                autoscaling.targetCpuUtilizationPercentage,
              )}
              onChange={set("autoscaling_target_cpu")}
            />
          </Field>
        </Grid>
      </Card>

      <Row>
        <Button variant="primary" disabled={!dirty || busy} onClick={save}>
          {busy ? "Applying…" : "Apply to the cluster"}
        </Button>
        <Button variant="ghost" disabled={!dirty || busy} onClick={() => setDraft({})}>
          Discard
        </Button>
        <Spacer />
        {dirty && <Muted>unapplied changes</Muted>}
      </Row>

      {conditions.length > 0 && (
        <Card title="Conditions" subtitle="what the operator last observed">
          <Stack gap={8}>
            {conditions.map((c) => (
              <Row key={c.type} gap={10} align="flex-start">
                <Dot tone={c.status === "True" ? "ok" : "warn"} size={7} />
                <Mono style={{ fontSize: 12, minWidth: 130 }}>{c.type}</Mono>
                <Muted>{c.message}</Muted>
              </Row>
            ))}
          </Stack>
        </Card>
      )}
    </Stack>
  );
}

function Stat({ label, value }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6, minWidth: 0 }}>
      <span
        style={{
          font: "500 10px/1 var(--sans)",
          color: "var(--fg-5)",
          letterSpacing: ".12em",
        }}
      >
        {label}
      </span>
      <div style={{ font: "400 13px/1.4 var(--sans)", color: "var(--fg)", minWidth: 0 }}>
        {value}
      </div>
    </div>
  );
}
