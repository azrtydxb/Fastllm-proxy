import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Button,
  Card,
  ErrorNote,
  Grid,
  Loading,
  Mono,
  Muted,
  Pill,
  Row,
  Spacer,
  Stack,
  fmtBytes,
  fmtDuration,
  fmtInt,
  fmtSnapshot,
} from "../ui.jsx";

// Two kinds of setting, kept visibly apart.
//
// The fallback model is **policy**: a row in the database, changed here, live
// on the next snapshot. Everything below it is a **flag** this process was
// started with — changing one is a deploy, not a click. Showing them in the
// same list with the same affordance would invite an operator to try editing
// something that cannot be edited; showing them as read-only pills next to
// their flag name says where to change it instead.
//
// They are all read from `GET /admin/config` rather than assumed. A settings
// screen that prints "5s" because that is the default is wrong the moment
// somebody passes `--config-poll 30`, and wrong in a way nobody checks.

export function Settings({ onUnauthorised, config }) {
  const [fallbackDraft, setFallbackDraft] = useState(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState(null);

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [models, fallback, cfg] = await Promise.all([
        api.get("/admin/provider-models"),
        api.get("/admin/fallback-model"),
        api.get("/admin/config"),
      ]);
      return { models, fallback, cfg };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const c = data.cfg || config || {};
  const selected =
    fallbackDraft ?? (data.fallback?.id ? String(data.fallback.id) : "");

  const saveFallback = async () => {
    const ok = await attempt(
      () =>
        api.put("/admin/fallback-model", {
          model_id: selected || null,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setFallbackDraft(null);
      reload();
    }
  };

  const danger = async (label, path, confirm) => {
    if (!window.confirm(confirm)) return;
    setBusy(true);
    try {
      const resp = await api.post(path);
      setNote(`${label}: ${JSON.stringify(resp)}`);
      setError(null);
      reload();
    } catch (e) {
      setError(e.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>
      {note && (
        <Card tone="accent">
          <Row>
            <Mono style={{ fontSize: 12 }}>{note}</Mono>
            <Spacer />
            <Button variant="small" onClick={() => setNote(null)}>
              dismiss
            </Button>
          </Row>
        </Card>
      )}

      <Card
        title="Deployment-wide fallback model"
        subtitle="appended to every chain — virtual or concrete — for the case a rule author did not anticipate. Authorised like any other candidate, so it never widens a caller's reach."
      >
        <Row gap={10}>
          <select
            style={{ width: 280 }}
            value={selected}
            onChange={(e) => setFallbackDraft(e.target.value)}
          >
            <option value="">none</option>
            {data.models.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </select>
          <Button variant="secondary" onClick={saveFallback}>
            Save
          </Button>
          {data.fallback?.name && <Muted>currently {data.fallback.name}</Muted>}
        </Row>
      </Card>

      <Grid cols={2}>
        <Card title="Planes & TLS">
          <Setting
            label="Role"
            hint="--role control / proxy / all"
            value={c.role}
            tone="neutral"
          />
          <Setting
            label="TLS on the admin listener"
            hint="/snapshot carries usable upstream credentials, so this must be on wherever a backend has a real one"
            value={c.tls ? "on" : "off"}
            tone={c.tls ? "ok" : "bad"}
          />
          <Setting
            label="Version"
            hint="the binary serving this page"
            value={c.version}
            tone="neutral"
          />
          <Setting
            label="Uptime"
            hint="of this control-plane process"
            value={fmtDuration(c.uptime_seconds)}
            tone="neutral"
          />
        </Card>

        <Card title="Snapshots">
          <Setting
            label="Snapshot version"
            hint="what this control plane last published — the version is a microsecond stamp, shown as its clock time"
            value={c.snapshot_version ? fmtSnapshot(c.snapshot_version) : "—"}
            tone="neutral"
          />
          <Setting
            label="Rebuild failures since start"
            hint="a write that committed but whose snapshot rebuild failed — the database and the published snapshot have diverged"
            value={fmtInt(c.snapshot_rebuild_failures)}
            tone={c.snapshot_rebuild_failures > 0 ? "bad" : "ok"}
          />
          <Setting
            label="Data-plane poll interval"
            hint="--config-poll — how long a change takes to reach a running proxy"
            value={`${c.config_poll_seconds}s`}
            tone="neutral"
          />
          <Setting
            label="Health report interval"
            hint="--health-report-interval — how often each replica reports what it can see"
            value={`${c.health_report_interval_seconds}s`}
            tone="neutral"
          />
          <Setting
            label="Routing policy"
            hint="--policy — which backend within a pool a request goes to. cache-affinity keeps a shared prefix on the node holding its KV cache; lowest-latency is for a pool whose members are not equivalent"
            value={c.policy || "—"}
            tone="neutral"
          />
          <Setting
            label="Outbound notifications"
            hint="--webhook-url — POSTs on backend up/down and snapshot rebuild failure. The URL is deliberately not shown: it is a capability, and this screen is readable by anyone holding usage:read"
            value={
              c.webhook_configured
                ? c.webhook_signed
                  ? "on · signed"
                  : "on · unsigned"
                : "off"
            }
            tone={
              c.webhook_configured
                ? c.webhook_signed
                  ? "ok"
                  : "warn"
                : "quiet"
            }
          />
        </Card>

        <Card title="Response cache">
          <Setting
            label="Enabled on"
            hint="per model via cache_ttl_seconds — never global"
            value={`${fmtInt(c.models_cached)} of ${fmtInt(c.models)} models`}
            tone="neutral"
          />
          <Setting
            label="Bounds"
            hint="--cache-max-entries / --cache-max-bytes, per process"
            value={`${fmtInt(c.cache_max_entries)} · ${fmtBytes(c.cache_max_bytes)}`}
            tone="neutral"
          />
          <Setting
            label="Dropped on snapshot change"
            hint="a reconfiguration can repoint a model at a different provider, and a cold cache beats a stale one"
            value="always"
            tone="ok"
          />
        </Card>

        <Card title="Pricing, sessions and telemetry">
          <Setting
            label="Unpriced models"
            hint="cost left NULL rather than assumed zero, so spend is reported as unknown instead of free"
            value={fmtInt(c.models_unpriced)}
            tone={c.models_unpriced > 0 ? "warn" : "ok"}
          />
          <Setting
            label="Session idle timeout"
            hint="the fastllm_session cookie — HttpOnly, no browser-held token"
            value={`${c.session_ttl_hours}h`}
            tone="neutral"
          />
          <Setting
            label="Classifier"
            hint="semantic routing tiers built into this binary"
            value={
              c.classifier_tier1
                ? c.classifier_tier2
                  ? "fast + refined"
                  : "fast only"
                : "not built"
            }
            tone={c.classifier_tier1 ? "ok" : "quiet"}
          />
          <Setting
            label="OTLP traces"
            hint="--otel-endpoint, in an otel build"
            value={
              c.otel_endpoint
                ? `on · 1 in ${fmtInt(c.otel_sample_one_in)}`
                : "off"
            }
            tone={c.otel_endpoint ? "ok" : "quiet"}
          />
        </Card>
      </Grid>

      <Card
        title="Danger zone"
        tone="bad"
        subtitle="both swap the routing table atomically — in-flight generations are unaffected"
      >
        <Row gap={10}>
          <Button
            variant="danger"
            disabled={busy}
            onClick={() =>
              danger(
                "Snapshot rebuilt",
                "/admin/snapshot/rebuild",
                "Rebuild and republish the snapshot now?",
              )
            }
          >
            Force snapshot rebuild
          </Button>
          <Button
            variant="danger"
            disabled={busy}
            onClick={() =>
              danger(
                "Sessions revoked",
                "/admin/sessions/revoke-all",
                "Log out every session, including this one. Continue?",
              )
            }
          >
            Revoke all sessions
          </Button>
          <Muted>
            Revoking sessions does not spare the caller — a route that kept its
            own session alive would be one an attacker holding a session could
            use to lock everyone else out. You will be signed out.
          </Muted>
        </Row>
      </Card>
    </Stack>
  );
}

function Setting({ label, hint, value, tone }) {
  return (
    <Row
      gap={12}
      style={{
        padding: "11px 0",
        borderBottom: "1px solid var(--line-soft)",
        flexWrap: "nowrap",
        alignItems: "flex-start",
      }}
    >
      <div style={{ minWidth: 0 }}>
        <div style={{ font: "500 12px var(--sans)", color: "var(--fg)" }}>
          {label}
        </div>
        <div
          style={{
            font: "400 10px/1.5 var(--sans)",
            color: "var(--fg-4)",
            textWrap: "pretty",
          }}
        >
          {hint}
        </div>
      </div>
      <Spacer />
      <Pill tone={tone} mono>
        {value ?? "—"}
      </Pill>
    </Row>
  );
}
