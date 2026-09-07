import React from "react";
import { api } from "../api.js";
import { useLoader } from "../load.js";
import { fleetSummary } from "../fleet.js";
import {
  Banner,
  Card,
  Dot,
  Ellipsis,
  Empty,
  ErrorNote,
  Grid,
  Loading,
  Mono,
  Muted,
  Pill,
  Row,
  Stack,
  Table,
  Tr,
  fmtBytes,
  fmtCompact,
  fmtDuration,
  fmtInt,
  fmtSnapshot,
  fmtSnapshotLag,
  hostOf,
  usePoll,
} from "../ui.jsx";

// One card per replica, and no fleet-wide roll-up of anything that does not
// legitimately roll up.
//
// This screen exists for the failure the aggregate hides: a replica stuck on
// an older snapshot answers /health with ok, lists the right models, and
// misbehaves only on whatever changed — most often a key it has never seen,
// which reaches the caller as an invalid key. Comparing snapshot_version
// across replicas is the only check that finds it, and it is the first thing
// on the page.

const POLL_MS = 5000;

const BACKEND_COLS = [
  { label: "BACKEND", width: "1.6fr" },
  { label: "HOST", width: "1.4fr" },
  { label: "STATE", width: ".9fr" },
  { label: "IN FLIGHT", width: ".7fr", align: "right" },
  { label: "REQUESTS", width: ".7fr", align: "right" },
  { label: "ERRORS", width: ".7fr", align: "right" },
];

const NODE_COLS = [
  { label: "NODE", width: "1.4fr" },
  { label: "ENDPOINTS", width: "1fr" },
  { label: "ENGINE", width: "1fr" },
  { label: "LEASE", width: "1.2fr" },
  { label: "LAST PROBED", width: "1.2fr" },
];

/** A timestamp as "3m ago" / "in 45s", which is what a lease is read as. */
function fmtWhen(iso) {
  const d = (new Date(iso) - new Date()) / 1000;
  const a = Math.abs(d);
  const n =
    a < 90
      ? `${Math.round(a)}s`
      : a < 5400
        ? `${Math.round(a / 60)}m`
        : `${Math.round(a / 3600)}h`;
  return d >= 0 ? `in ${n}` : `${n} ago`;
}

export function Fleet({ onUnauthorised, config }) {
  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [fleet, nodes] = await Promise.all([
        api.get("/admin/fleet"),
        api.get("/admin/nodes"),
      ]);
      return { fleet, nodes };
    },
    {
      onUnauthorised,
    },
  );
  usePoll(reload, POLL_MS);

  if (loading && !data) return <Loading />;
  const reports = data?.fleet || [];
  const nodes = data?.nodes || [];
  const summary = fleetSummary(reports);

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      {/* The other half of the fleet. A proxy reports what it can reach; an
          agent decides what there is to reach at all — it registers this
          host's endpoints and holds them on a lease. Nothing showed them, so a
          stopped agent looked like nothing until its leases lapsed and the
          endpoints quietly went away. */}
      <Card
        title="Registration agents"
        subtitle="hosts registering their own endpoints, held on a lease"
      >
        {nodes.length === 0 ? (
          <Empty>
            No host is registering endpoints. Providers added by hand are
            unaffected — this is only about agents.
          </Empty>
        ) : (
          <Table cols={NODE_COLS}>
            {nodes.map((n) => {
              const lapsed =
                !n.lease_expires_at ||
                new Date(n.lease_expires_at) < new Date();
              return (
                <Tr
                  key={n.node}
                  cols={NODE_COLS}
                  cells={[
                    <Row key="n" gap={8} style={{ flexWrap: "nowrap" }}>
                      <Dot
                        tone={lapsed ? "bad" : n.degraded > 0 ? "warn" : "ok"}
                      />
                      <Mono style={{ font: "500 12px var(--mono)" }}>
                        {n.node}
                      </Mono>
                    </Row>,
                    <Mono key="e" style={{ font: "400 12px var(--mono)" }}>
                      {n.endpoints}
                      {n.degraded > 0 ? ` · ${n.degraded} degraded` : ""}
                    </Mono>,
                    <Mono
                      key="g"
                      style={{
                        font: "400 12px var(--mono)",
                        color: "var(--fg-3)",
                      }}
                    >
                      {n.engines.length ? n.engines.join(", ") : "—"}
                    </Mono>,
                    // The lease is the agent's own heartbeat: `register` pushes
                    // it forward every check-in, so "in the future" means the
                    // agent is alive. Lapsed is what a stopped agent looks like.
                    <Mono
                      key="l"
                      style={{
                        font: "400 12px var(--mono)",
                        color: lapsed ? "var(--bad-fg)" : "var(--fg-3)",
                      }}
                    >
                      {lapsed
                        ? "lapsed"
                        : `renews ${fmtWhen(n.lease_expires_at)}`}
                    </Mono>,
                    // A separate signal: the control plane's own probe, not the
                    // agent's word. Stale here with a live lease means the
                    // sweep is not running.
                    <Mono
                      key="p"
                      style={{
                        font: "400 12px var(--mono)",
                        color: "var(--fg-4)",
                      }}
                    >
                      {n.last_probed_at ? fmtWhen(n.last_probed_at) : "never"}
                    </Mono>,
                  ]}
                />
              );
            })}
          </Table>
        )}
      </Card>

      {reports.length === 0 && (
        <Banner tone="warn">
          No replica is reporting.
          <span style={{ color: "var(--fg-3)", fontWeight: 400 }}>
            {" "}
            Proxies push health every{" "}
            {config?.health_report_interval_seconds ?? 10}s over the proxy
            token, and a replica that stops is dropped after 30 seconds rather
            than shown stale — &ldquo;up, 40 minutes ago&rdquo; is not health.
          </span>
        </Banner>
      )}

      {summary.snapshotSpread > 0 && (
        <Banner tone="bad">
          {/* Each laggard's own version, not `max - spread`: that expression is
              the minimum, so with three distinct versions it reported every
              behind replica as being on the oldest one. */}
          {reports
            .filter((r) => summary.laggards.includes(r.replica))
            .map(
              (r) =>
                `${r.replica} (${fmtSnapshotLag(r.snapshot_version, summary.snapshotVersion)})`,
            )
            .join(", ")}{" "}
          {summary.laggards.length === 1 ? "is" : "are"} behind the
          fleet&rsquo;s snapshot of {fmtSnapshot(summary.snapshotVersion)}
          <span style={{ color: "var(--fg-3)", fontWeight: 400 }}>
            {" "}
            — it answers /health with ok and lists the right models, and
            misbehaves only on whatever changed.
          </span>
        </Banner>
      )}

      {summary.backendsSplit.length > 0 && (
        <Banner tone="bad">
          Replicas disagree about{" "}
          {summary.backendsSplit.map((b) => b.model).join(", ")}
          <span style={{ color: "var(--fg-3)", fontWeight: 400 }}>
            {" "}
            — reachable from {summary.backendsSplit[0].healthyOn.join(", ")} and
            not from {summary.backendsSplit[0].unhealthyOn.join(", ")}. That is
            a network partition, not a dead backend, and a fleet-wide average
            would have hidden it.
          </span>
        </Banner>
      )}

      {/* Sized by the card, not by the count: a one-replica fleet in a
          three-column grid used to stretch a single card across the whole
          window with its metrics metres apart. */}
      <Grid cols="repeat(auto-fill, minmax(330px, 1fr))">
        {reports.map((r) => {
          const lagging = summary.laggards.includes(r.replica);
          const up = (r.backends || []).filter((b) => b.healthy).length;
          const p = r.process || {};
          const lookups = (p.cache_hits || 0) + (p.cache_misses || 0);
          const hit =
            lookups > 0
              ? ((p.cache_hits / lookups) * 100).toFixed(0) + "%"
              : "—";
          return (
            <Card key={r.replica} tone={lagging ? "bad" : undefined}>
              <Stack gap={12}>
                <Row style={{ flexWrap: "nowrap" }}>
                  <Row gap={8} style={{ flexWrap: "nowrap", minWidth: 0 }}>
                    <Dot tone={lagging ? "bad" : "ok"} />
                    <Ellipsis
                      style={{ font: "500 13px var(--mono)" }}
                      title={r.replica}
                    >
                      {r.replica}
                    </Ellipsis>
                  </Row>
                  <div style={{ flex: 1 }} />
                  <Pill tone={lagging ? "bad" : "ok"} mono>
                    <span title={`snapshot version ${r.snapshot_version}`}>
                      {fmtSnapshot(r.snapshot_version)}
                    </span>
                  </Pill>
                </Row>
                <Muted>
                  up {fmtDuration(r.uptime_seconds)} · {up} of{" "}
                  {(r.backends || []).length} backends reachable
                </Muted>
                <Grid
                  cols={2}
                  gap={10}
                  style={{
                    paddingTop: 10,
                    borderTop: "1px solid var(--line-mid)",
                  }}
                >
                  <Metric
                    label="SERVED"
                    value={fmtCompact(p.requests_ok || 0)}
                  />
                  <Metric
                    label="FAILED"
                    value={fmtCompact(p.requests_failed || 0)}
                    tone={p.requests_failed > 0 ? "warn" : undefined}
                  />
                  <Metric
                    label="CACHE HIT"
                    value={hit}
                    hint={`${fmtInt(p.cache_entries || 0)} held`}
                  />
                  <Metric
                    label="CACHE SIZE"
                    value={fmtBytes(p.cache_bytes || 0)}
                  />
                  <Metric
                    label="USAGE DROPPED"
                    value={fmtInt(p.usage_dropped || 0)}
                    tone={p.usage_dropped > 0 ? "bad" : undefined}
                  />
                  <Metric
                    label="IN FLIGHT"
                    value={fmtInt(
                      (r.backends || []).reduce((a, b) => a + b.inflight, 0),
                    )}
                  />
                </Grid>
                {lagging && (
                  <Muted style={{ color: "var(--bad-fg)" }}>
                    stuck on an older snapshot ·{" "}
                    {fmtSnapshotLag(
                      r.snapshot_version,
                      summary.snapshotVersion,
                    )}
                  </Muted>
                )}
              </Stack>
            </Card>
          );
        })}
      </Grid>

      {summary.backends.length > 0 && (
        <Card
          title="Backends, merged"
          subtitle="counters sum across replicas; health does not — a backend is up only where every replica agrees"
        >
          <Table cols={BACKEND_COLS}>
            {summary.backends.map((b) => (
              <Tr
                key={b.key}
                cols={BACKEND_COLS}
                cells={[
                  <Row
                    key="n"
                    gap={8}
                    style={{ flexWrap: "nowrap", minWidth: 0 }}
                  >
                    <Dot tone={b.split ? "warn" : b.healthy ? "ok" : "bad"} />
                    <Ellipsis style={{ font: "400 12px var(--mono)" }}>
                      {b.model}
                    </Ellipsis>
                  </Row>,
                  <Ellipsis
                    key="h"
                    title={b.api_base}
                    style={{
                      font: "400 11px var(--mono)",
                      color: "var(--fg-4)",
                    }}
                  >
                    {hostOf(b.api_base)}
                  </Ellipsis>,
                  b.split ? (
                    <Pill key="s" tone="warn">
                      split {b.healthyOn.length}/{b.reporting}
                    </Pill>
                  ) : (
                    <Pill key="s" tone={b.healthy ? "ok" : "bad"}>
                      {b.healthy ? `up on ${b.reporting}` : "down"}
                    </Pill>
                  ),
                  <Mono
                    key="i"
                    style={{
                      font: "400 12px var(--mono)",
                      color: "var(--fg-2)",
                    }}
                  >
                    {b.inflight}
                  </Mono>,
                  <Mono
                    key="r"
                    style={{
                      font: "400 12px var(--mono)",
                      color: "var(--fg-2)",
                    }}
                  >
                    {fmtCompact(b.requests)}
                  </Mono>,
                  <Mono
                    key="e"
                    style={{
                      font: "400 12px var(--mono)",
                      color: b.errors > 0 ? "var(--warn-fg)" : "var(--fg-4)",
                    }}
                  >
                    {fmtInt(b.errors)}
                  </Mono>,
                ]}
              />
            ))}
          </Table>
        </Card>
      )}

      <Grid cols={3}>
        <Card title="Why per replica">
          <Muted>
            A lagging replica is invisible in the aggregate: it answers /health
            with ok and only misbehaves on whatever part of the snapshot
            changed. Comparing{" "}
            <Mono style={{ color: "var(--fg-2)" }}>snapshot_version</Mono>{" "}
            across replicas is the check that finds it.
          </Muted>
        </Card>
        <Card title="What does not sum">
          <Muted>
            Percentiles need merged histograms, not an average of four p99s. The
            response cache is per process, so a freshly restarted replica drags
            a naive fleet hit rate down while nothing is wrong — which is why
            the hit rate is shown per replica here.
          </Muted>
        </Card>
        <Card title="Nothing is stored">
          <Muted>
            These reports live in the control plane&rsquo;s memory and are lost
            on restart. Health is a statement about now; a row saying a backend
            was up two hours ago is history nobody asked for. A replica that
            stops reporting ages out after 30 seconds.
          </Muted>
        </Card>
      </Grid>
    </Stack>
  );
}

function Metric({ label, value, hint, tone }) {
  return (
    <div>
      <div
        style={{
          font: "400 9.5px var(--sans)",
          color: "var(--fg-5)",
          letterSpacing: ".06em",
        }}
      >
        {label}
      </div>
      <div
        style={{
          font: "500 13px var(--mono)",
          color:
            tone === "bad"
              ? "var(--bad-fg)"
              : tone === "warn"
                ? "var(--warn-fg)"
                : "var(--fg)",
        }}
      >
        {value}
        {hint && (
          <span style={{ font: "400 10px var(--sans)", color: "var(--fg-5)" }}>
            {" "}
            · {hint}
          </span>
        )}
      </div>
    </div>
  );
}
