import React, { useEffect, useRef, useState } from "react";
import { api, query } from "../api.js";
import { useLoader } from "../load.js";
import { fleetSummary, rateBetween, backendKey } from "../fleet.js";
import { Legend, RANGES, TimeChart, useTimeseries } from "../charts.jsx";
import { TimeseriesModal } from "./TimeseriesModal.jsx";
import {
  Bar,
  Banner,
  Button,
  Card,
  Dot,
  Ellipsis,
  Empty,
  ErrorNote,
  Grid,
  Loading,
  Mono,
  Muted,
  NotAvailable,
  Pill,
  Row,
  Spacer,
  Spark,
  Stack,
  Table,
  Tr,
  fmtCompact,
  fmtInt,
  fmtMoney,
  fmtSnapshot,
  fmtWhen,
  hostOf,
  usePoll,
} from "../ui.jsx";

const POLL_MS = 5000;

const BACKEND_COLS = [
  { label: "BACKEND", width: "1.7fr" },
  { label: "PROTOCOL", width: ".7fr" },
  { label: "IN FLIGHT", width: "1.2fr" },
  { label: "REQUESTS", width: ".8fr", align: "right" },
  { label: "ERRORS", width: ".7fr", align: "right" },
];

export function Overview({ onUnauthorised, config, go }) {
  const [series, setSeries] = useState([]);
  const sample = useRef(null);
  // See Metrics: keyed on the poll, not on the counter, or an idle fleet
  // holds its last measured rate on screen forever.
  const [polls, setPolls] = useState(0);

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [fleet, models, traffic, events] = await Promise.all([
        api.get("/admin/fleet"),
        api.get("/admin/provider-models"),
        // Asked for wide and ranked here: the route orders by spend, so a
        // free self-hosted model carrying most of the traffic sorts last and a
        // limit of 6 would drop it out of a panel about *traffic*.
        api.get(
          "/admin/usage" + query({ group_by: "frontend_model", limit: 100 }),
        ),
        api.get("/admin/audit" + query({ limit: 6 })),
      ]);
      return {
        fleet,
        models,
        events,
        traffic: [...traffic]
          .sort((a, b) => b.requests - a.requests)
          .slice(0, 6),
      };
    },
    { onUnauthorised },
  );

  usePoll(async () => {
    await reload();
    setPolls((n) => n + 1);
  }, POLL_MS);

  // A request rate this page measured itself, from two samples of a counter
  // the fleet reports. There is no stored history to ask for one; this is the
  // honest alternative, and the chart says so. Sampled in an effect rather
  // than during render so a re-render for any other reason cannot fabricate a
  // sample interval of zero.
  const requestsTotal = data ? fleetSummary(data.fleet).requests : null;
  useEffect(() => {
    if (requestsTotal === null) return;
    const now = Date.now();
    const prev = sample.current;
    sample.current = { at: now, value: requestsTotal };
    const rps = rateBetween(
      prev,
      sample.current,
      prev ? (now - prev.at) / 1000 : 0,
    );
    if (rps !== null) setSeries((s) => [...s, rps].slice(-40));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [polls, requestsTotal === null]);

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const summary = fleetSummary(data.fleet);

  // Protocol lives in the model configuration, not in a health report — the
  // report carries what a proxy observed, and the protocol is not something it
  // observes. Joined here rather than added to the report for that reason.
  const protocolOf = new Map();
  for (const m of data.models || []) {
    for (const b of m.backends || []) {
      protocolOf.set(
        backendKey(b.api_base, b.upstream_model || m.name),
        b.protocol,
      );
    }
  }

  const rps = series.length ? series[series.length - 1] : null;

  const totalTraffic = (data.traffic || []).reduce((a, r) => a + r.requests, 0);
  const spend = (data.traffic || []).reduce((a, r) => a + r.cost_micros, 0);
  const unpriced = (data.traffic || []).reduce(
    (a, r) => a + r.unpriced_requests,
    0,
  );

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      {summary.replicas === 0 && (
        <Banner tone="warn">
          No proxy has reported in.
          <span style={{ color: "var(--fg-3)", fontWeight: 400 }}>
            {" "}
            Health lives in the data plane; a replica publishes it every{" "}
            {config?.health_report_interval_seconds ?? 10}s over the proxy
            token. Nothing here is stale — there is nothing yet.
          </span>
        </Banner>
      )}

      {summary.snapshotSpread > 0 && (
        <Banner
          tone="bad"
          action={
            <Button variant="small" onClick={() => go("fleet")}>
              Inspect fleet
            </Button>
          }
        >
          {summary.laggards.join(", ")}{" "}
          {summary.laggards.length === 1 ? "is" : "are"} serving an older
          snapshot
          <span style={{ color: "var(--fg-3)", fontWeight: 400 }}>
            {" "}
            — a lagging replica answers /health with ok and misbehaves only on
            whatever changed, most often a key it has never seen.
          </span>
        </Banner>
      )}

      {summary.backendsSplit.length > 0 && (
        <Banner
          tone="bad"
          action={
            <Button variant="small" onClick={() => go("fleet")}>
              Inspect fleet
            </Button>
          }
        >
          Replicas disagree about {summary.backendsSplit.length} backend
          {summary.backendsSplit.length === 1 ? "" : "s"}
          <span style={{ color: "var(--fg-3)", fontWeight: 400 }}>
            {" "}
            — one proxy cannot reach what the others can, which is a partition
            rather than a dead backend.
          </span>
        </Banner>
      )}

      <Grid cols={4}>
        <Kpi
          label="REQUESTS/S"
          value={rps === null ? "—" : rps.toFixed(rps < 10 ? 2 : 0)}
          unit="rps"
          series={series}
          foot={
            rps === null
              ? "measuring — needs two polls"
              : `summed across ${summary.replicas} reporting replica${summary.replicas === 1 ? "" : "s"}`
          }
        />
        <Kpi
          label="BACKENDS UP"
          value={`${summary.backendsUp}`}
          unit={`of ${summary.backends.length}`}
          tone={summary.backendsUp === summary.backends.length ? "ok" : "bad"}
          foot="up only where every replica agrees"
        />
        <Kpi
          label="ERROR RATE"
          value={
            summary.requests > 0
              ? ((summary.errors / summary.requests) * 100).toFixed(2)
              : "—"
          }
          unit="%"
          tone={summary.errors > 0 ? "warn" : "ok"}
          foot={`${fmtInt(summary.errors)} of ${fmtInt(summary.requests)} since each replica started`}
        />
        <Kpi
          label="IN FLIGHT"
          value={fmtInt(summary.inflight)}
          unit="requests"
          foot={
            summary.snapshotVersion
              ? `snapshot ${fmtSnapshot(summary.snapshotVersion)}${summary.snapshotSpread ? " · fleet split" : " · fleet in sync"}`
              : "no replica reporting"
          }
          tone={summary.snapshotSpread ? "warn" : "accent"}
        />
      </Grid>

      <TrafficHistory />

      <Grid cols="1.35fr minmax(0,1fr)">
        <Card
          title="Backends"
          right={
            <Row gap={10}>
              <Muted>live from each proxy · merged by (api_base, model)</Muted>
              {summary.snapshotVersion !== null && (
                <Pill tone={summary.snapshotSpread ? "bad" : "ok"} mono>
                  <span title={`snapshot version ${summary.snapshotVersion}`}>
                    {summary.snapshotSpread
                      ? `${fmtSnapshot(summary.snapshotVersion)} · ${summary.laggards.length} behind`
                      : `${fmtSnapshot(summary.snapshotVersion)} · in sync`}
                  </span>
                </Pill>
              )}
            </Row>
          }
        >
          {summary.backends.length === 0 ? (
            <Empty>
              Nothing reported yet. Backends appear here once a proxy replica
              has probed them.
            </Empty>
          ) : (
            <Table cols={BACKEND_COLS}>
              {summary.backends.map((b) => (
                <Tr
                  key={b.key}
                  cols={BACKEND_COLS}
                  cells={[
                    <Row
                      gap={8}
                      key="n"
                      style={{ flexWrap: "nowrap", minWidth: 0 }}
                    >
                      <Dot tone={b.split ? "warn" : b.healthy ? "ok" : "bad"} />
                      <div style={{ minWidth: 0 }}>
                        <Ellipsis
                          style={{ font: "500 12px/1.2 var(--sans)" }}
                          title={b.model}
                        >
                          {b.model}
                        </Ellipsis>
                        <Ellipsis
                          style={{
                            font: "400 10px/1.3 var(--mono)",
                            color: "var(--fg-4)",
                          }}
                          title={b.api_base}
                        >
                          {hostOf(b.api_base)}
                        </Ellipsis>
                      </div>
                    </Row>,
                    <Mono
                      key="p"
                      style={{
                        font: "400 11px var(--mono)",
                        color: "var(--fg-3)",
                      }}
                    >
                      {protocolOf.get(b.key) || "—"}
                    </Mono>,
                    <Row key="i" gap={8} style={{ flexWrap: "nowrap" }}>
                      <Bar
                        pct={Math.min(100, b.inflight * 10)}
                        height={5}
                        tone={b.healthy ? "accent" : "bad"}
                      />
                      <Mono
                        style={{
                          font: "400 11px var(--mono)",
                          color: "var(--fg-2)",
                        }}
                      >
                        {b.inflight}
                      </Mono>
                    </Row>,
                    <Mono
                      key="r"
                      style={{
                        font: "400 11px var(--mono)",
                        color: "var(--fg-2)",
                      }}
                    >
                      {fmtCompact(b.requests)}
                    </Mono>,
                    <Mono
                      key="e"
                      style={{
                        font: "400 11px var(--mono)",
                        color: b.errors > 0 ? "var(--warn-fg)" : "var(--fg-4)",
                      }}
                    >
                      {fmtInt(b.errors)}
                    </Mono>,
                  ]}
                />
              ))}
            </Table>
          )}
          <div style={{ marginTop: 12 }}>
            <Muted>
              Time to first token per backend is not here:{" "}
              <NotAvailable why="Latency histograms are per process, on each proxy's own /metrics. Merging percentiles needs the buckets, not the computed quantile — an average of four p99s is not a p99.">
                percentiles do not merge
              </NotAvailable>
              . The Metrics screen says what to scrape instead.
            </Muted>
          </div>
        </Card>

        <Stack gap={14}>
          <Card
            title="Traffic by frontend model"
            subtitle="what callers asked for, before routing decided"
          >
            {(data.traffic || []).length === 0 ? (
              <Empty>No usage recorded yet.</Empty>
            ) : (
              (data.traffic || []).map((row, i) => {
                const share = totalTraffic
                  ? (row.requests / totalTraffic) * 100
                  : 0;
                return (
                  <div
                    key={row.key || i}
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: 6,
                      marginBottom: 12,
                    }}
                  >
                    <Row gap={8} style={{ flexWrap: "nowrap" }}>
                      <Ellipsis style={{ font: "400 12px var(--mono)" }}>
                        {row.key || "(unnamed)"}
                      </Ellipsis>
                      <Spacer />
                      <Muted>{share.toFixed(0)}%</Muted>
                    </Row>
                    <Bar
                      pct={share}
                      tone={
                        ["accent", "ok", "violet", "warn", "muted", "muted"][
                          i
                        ] || "muted"
                      }
                    />
                  </div>
                );
              })
            )}
            {spend > 0 || unpriced > 0 ? (
              <div
                style={{
                  paddingTop: 10,
                  borderTop: "1px solid var(--line-mid)",
                }}
              >
                <Muted>
                  {fmtMoney(spend)} across these
                  {unpriced > 0 && (
                    <>
                      {" · "}
                      <span style={{ color: "var(--warn-fg)" }}>
                        {fmtInt(unpriced)} unpriced request
                        {unpriced === 1 ? "" : "s"}
                      </span>{" "}
                      contribute nothing to that total
                    </>
                  )}
                </Muted>
              </div>
            ) : null}
          </Card>

          <Card
            title="Control-plane changes"
            subtitle="the audit log's head — reads and rejected attempts are not recorded"
            right={
              <Button variant="small" onClick={() => go("audit")}>
                all
              </Button>
            }
          >
            {(data.events || []).length === 0 ? (
              <Empty>Nothing has changed yet.</Empty>
            ) : (
              (data.events || []).map((e) => (
                <div
                  key={e.id}
                  style={{
                    display: "flex",
                    gap: 10,
                    padding: "8px 0",
                    borderBottom: "1px solid var(--line-soft)",
                  }}
                >
                  <Dot
                    tone={
                      e.action === "DELETE"
                        ? "bad"
                        : e.action === "POST"
                          ? "ok"
                          : "accent"
                    }
                    style={{ marginTop: 5 }}
                  />
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <Ellipsis
                      style={{
                        font: "400 12px/1.4 var(--sans)",
                        color: "var(--fg-2)",
                      }}
                      title={`${e.action} ${e.target}`}
                    >
                      <Mono style={{ color: "var(--fg-3)" }}>{e.action}</Mono>{" "}
                      {e.target}
                    </Ellipsis>
                    <div
                      style={{
                        font: "400 10px/1.4 var(--mono)",
                        color: "var(--fg-5)",
                      }}
                    >
                      {fmtWhen(e.at)} · {e.actor_name}
                    </div>
                  </div>
                </div>
              ))
            )}
          </Card>
        </Stack>
      </Grid>
    </Stack>
  );
}

function Kpi({ label, value, unit, series, foot, tone = "accent" }) {
  return (
    <Card style={{ padding: "16px 16px 12px" }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        <span
          style={{
            font: "400 11px/1 var(--sans)",
            color: "var(--fg-3)",
            letterSpacing: ".06em",
          }}
        >
          {label}
        </span>
        <div style={{ display: "flex", alignItems: "baseline", gap: 6 }}>
          <span
            style={{ font: "500 26px/1 var(--mono)", letterSpacing: "-.02em" }}
          >
            {value}
          </span>
          <span
            style={{ font: "400 12px/1 var(--mono)", color: "var(--fg-4)" }}
          >
            {unit}
          </span>
        </div>
        {series ? (
          <Spark values={series} tone={tone} />
        ) : (
          <div style={{ height: 38 }} />
        )}
        <div style={{ font: "400 10px/1.4 var(--sans)", color: "var(--fg-5)" }}>
          {foot}
        </div>
      </div>
    </Card>
  );
}

/**
 * The last 24 hours, from the database rather than from counters diffed in
 * this tab.
 *
 * The KPI tiles above are deliberately still live-since-load: they answer
 * "what is happening right now", and a rate measured between two polls is
 * the honest way to get that. This answers the different question the old
 * UI could not — "what has been happening" — and it survives a reload,
 * because the history is on the server.
 *
 * Fixed to 24h with no controls of its own. A dashboard tile that carries a
 * range picker, filters and pagination has stopped being glanceable; all of
 * that lives in the modal, one click away.
 */
function TrafficHistory() {
  const [open, setOpen] = useState(false);
  const range = RANGES.find((r) => r.id === "24h");
  const { points, error } = useTimeseries({ range });

  const series = [
    { key: "requests_ok", label: "served", tone: "accent" },
    { key: "upstream_errors", label: "upstream errors", tone: "bad" },
    { key: "refused", label: "refused", tone: "warn" },
  ];

  // `requests` already includes the failures, so the served band has to be
  // the remainder or the stack double-counts every error.
  const shaped = points?.map((p) => {
    const refused =
      p.refused_authorisation +
      p.refused_rate_limit +
      p.refused_budget +
      p.refused_no_backend +
      // Never reached attribution, so it is not inside `requests` and is
      // added to the stack rather than carved out of the served band.
      p.refused_unattributed;
    return {
      ...p,
      refused,
      requests_ok: Math.max(
        0,
        p.requests - p.upstream_errors - (refused - p.refused_unattributed),
      ),
    };
  });

  const nothing = shaped && shaped.every((p) => p.requests === 0);

  return (
    <>
      <Card
        title="Last 24 hours"
        right={
          <Row gap={10}>
            <Muted>from usage_events · click to explore</Muted>
            <Button onClick={() => setOpen(true)}>expand</Button>
          </Row>
        }
      >
        <div
          onClick={() => setOpen(true)}
          style={{ cursor: "pointer" }}
          role="button"
          tabIndex={0}
          onKeyDown={(e) =>
            (e.key === "Enter" || e.key === " ") && setOpen(true)
          }
          aria-label="Open traffic history"
        >
          {error ? (
            <NotAvailable why={error}>
              History comes from <Mono>GET /admin/timeseries</Mono>. The message
              above is what the request actually returned — a 404 means this
              control plane predates the endpoint, anything else is a live
              failure worth reading the server logs for.
            </NotAvailable>
          ) : (
            <>
              <TimeChart
                points={shaped}
                series={series}
                height={120}
                spanSeconds={range.seconds}
              />
              <div style={{ height: 8 }} />
              <Row gap={12}>
                <Legend series={series} />
                <Spacer />
                {nothing && <Muted>nothing served in this window</Muted>}
              </Row>
            </>
          )}
        </div>
      </Card>
      {open && <TimeseriesModal onClose={() => setOpen(false)} />}
    </>
  );
}
