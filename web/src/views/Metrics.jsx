import React, { useEffect, useRef, useState } from "react";
import { api } from "../api.js";
import { useLoader } from "../load.js";
import { fleetSummary, rateBetween } from "../fleet.js";
import { Legend, RANGES, TimeChart, useTimeseries } from "../charts.jsx";
import { TimeseriesModal } from "./TimeseriesModal.jsx";
import {
  Bar,
  Button,
  Card,
  Donut,
  Dot,
  Ellipsis,
  Empty,
  ErrorNote,
  Grid,
  Label,
  Loading,
  Mono,
  NotAvailable,
  Muted,
  Row,
  Spark,
  Stack,
  Table,
  Tr,
  Unmeasured,
  fmtBytes,
  fmtCompact,
  fmtInt,
  hostOf,
  usePoll,
} from "../ui.jsx";

// What this screen is, and deliberately is not.
//
// There is no metrics store behind the control plane. `/metrics` is a
// Prometheus scrape of current values on each proxy, and `usage_events` is one
// row per request — neither is a time series a browser can query for "the last
// 24 hours". A charting screen built on top of that would have to invent its
// history, so this one does not: every line here is a rate this page computed
// from counters it watched change while it was open, and the header says so.
//
// The panels that genuinely need stored history — TTFT percentiles over time,
// jitter, per-model latency — are not drawn empty or filled with zeroes. They
// name the metric and the query that answers them in Prometheus, which is
// where that history actually lives.

const POLL_MS = 5000;
const WINDOW = 40;

export function Metrics({ onUnauthorised, config }) {
  const [scope, setScope] = useState("fleet");
  const [series, setSeries] = useState({ rps: [], errors: [], cache: [] });
  const last = useRef(null);
  // Counts polls rather than watching the totals: an idle fleet reports the
  // same counters every time, and an effect keyed on those values is skipped
  // — which froze every rate at its last non-zero reading instead of falling
  // to zero, for as long as the page stayed open.
  const [polls, setPolls] = useState(0);

  const { data, error, loading, reload, setError } = useLoader(
    () => api.get("/admin/fleet"),
    { onUnauthorised },
  );
  usePoll(async () => {
    await reload();
    setPolls((n) => n + 1);
  }, POLL_MS);

  const reports = data || [];
  const scoped = scope === "fleet" ? reports : reports.filter((r) => r.replica === scope);
  const summary = fleetSummary(scoped);

  const cacheHits = scoped.reduce((a, r) => a + (r.process?.cache_hits || 0), 0);
  const cacheMisses = scoped.reduce((a, r) => a + (r.process?.cache_misses || 0), 0);
  const dropped = scoped.reduce((a, r) => a + (r.process?.usage_dropped || 0), 0);

  // Rates, sampled once per poll. Reset when the scope changes: splicing one
  // replica's rate onto the fleet's line would draw a cliff that never
  // happened.
  const key = scope;
  useEffect(() => {
    setSeries({ rps: [], errors: [], cache: [] });
    last.current = null;
  }, [key]);

  const totals = data
    ? { requests: summary.requests, errors: summary.errors, cache: cacheHits + cacheMisses }
    : null;
  useEffect(() => {
    if (!totals) return;
    const now = Date.now();
    const prev = last.current;
    last.current = { at: now, ...totals };
    if (!prev) return;
    const secs = (now - prev.at) / 1000;
    const push = (from, to) => rateBetween({ value: from }, { value: to }, secs);
    const rps = push(prev.requests, totals.requests);
    const eps = push(prev.errors, totals.errors);
    const cps = push(prev.cache, totals.cache);
    setSeries((s) => ({
      rps: rps === null ? s.rps : [...s.rps, rps].slice(-WINDOW),
      errors: eps === null ? s.errors : [...s.errors, eps].slice(-WINDOW),
      cache: cps === null ? s.cache : [...s.cache, cps].slice(-WINDOW),
    }));
    // Keyed on the poll counter: a poll that changed nothing must still
    // advance the line, because a flat line is a fact about the system.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [polls]);

  if (loading && !data) return <Loading />;

  // The scope survives in state after the replica it names stops reporting and
  // drops out of the fleet. Rendering the empty selection would print zeros
  // under that replica's name as though they had been measured.
  const goneScope = scope !== "fleet" && reports.length > 0 && scoped.length === 0;

  const hitRate = cacheHits + cacheMisses > 0 ? (cacheHits / (cacheHits + cacheMisses)) * 100 : null;
  const now = (arr) => (arr.length ? arr[arr.length - 1] : null);

  const worstBackends = [...summary.backends]
    .filter((b) => b.requests > 0 || b.errors > 0)
    .sort((a, b) => b.errorRate - a.errorRate || b.errors - a.errors)
    .slice(0, 6);

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row gap={8}>
        <Label>SCOPE</Label>
        <ScopeTab active={scope === "fleet"} onClick={() => setScope("fleet")}>
          Fleet
        </ScopeTab>
        {reports.map((r) => (
          <ScopeTab key={r.replica} active={scope === r.replica} onClick={() => setScope(r.replica)}>
            {r.replica}
          </ScopeTab>
        ))}
        <div style={{ flex: 1 }} />
        <Muted>
          {scope === "fleet"
            ? `Counters summed across ${summary.replicas} reporting replica${summary.replicas === 1 ? "" : "s"}.`
            : `${scope} alone.`}{" "}
          Rates are measured by this page, every {POLL_MS / 1000}s since it loaded.
        </Muted>
      </Row>

      {goneScope && (
        <Card tone="warn" title={`${scope} is no longer reporting`}>
          <Muted>
            It aged out of the fleet — a replica that stops reporting is dropped after 30 seconds
            rather than shown stale. Nothing below is measured for it.{" "}
            <Button variant="small" onClick={() => setScope("fleet")}>
              back to the fleet
            </Button>
          </Muted>
        </Card>
      )}

      <MetricsHistory />

      <Grid cols={3}>
        <RateCard
          title="Requests per second"
          sub="counter deltas from each replica's report"
          value={now(series.rps)}
          unit="rps"
          series={series.rps}
          tone="accent"
          digits={2}
        />
        <RateCard
          title="Upstream errors per second"
          sub="a backend answering 5xx, before failover"
          value={now(series.errors)}
          unit="eps"
          series={series.errors}
          tone={now(series.errors) > 0 ? "bad" : "ok"}
          digits={3}
        />
        <RateCard
          title="Cache lookups per second"
          sub="hits plus misses, per process"
          value={now(series.cache)}
          unit="lps"
          series={series.cache}
          tone="violet"
          digits={2}
        />
      </Grid>

      <Grid cols="minmax(0,1fr) minmax(0,1.2fr)">
        <Card
          title="Response cache"
          subtitle={
            scope === "fleet"
              ? "Σhits / Σ(hits + misses) — recomputed from the totals, never an average of rates"
              : "this process's own cache"
          }
        >
          {hitRate === null ? (
            <Empty>No cache lookups yet. The cache is opt-in per model, via cache_ttl_seconds.</Empty>
          ) : (
            <Row gap={22} style={{ flexWrap: "nowrap" }}>
              <Donut
                pct={hitRate}
                label={`${hitRate.toFixed(0)}%`}
                sub="HIT"
                tone={hitRate > 30 ? "ok" : "accent"}
              />
              <Stack gap={10} style={{ flex: 1 }}>
                <Stat label="Hits" value={fmtCompact(cacheHits)} />
                <Stat label="Misses" value={fmtCompact(cacheMisses)} />
                <Stat
                  label="Held"
                  value={`${fmtInt(scoped.reduce((a, r) => a + (r.process?.cache_entries || 0), 0))} entries`}
                  hint={fmtBytes(scoped.reduce((a, r) => a + (r.process?.cache_bytes || 0), 0))}
                />
              </Stack>
            </Row>
          )}
          {scope === "fleet" && reports.length > 1 && (
            <div style={{ marginTop: 14, paddingTop: 12, borderTop: "1px solid var(--line-mid)" }}>
              <Muted>
                Per replica, because the cache is per process — a worker restarted minutes ago has a
                cold one and nothing is wrong:
              </Muted>
              <Row gap={8} style={{ marginTop: 8 }}>
                {reports.map((r) => {
                  const h = r.process?.cache_hits || 0;
                  const m = r.process?.cache_misses || 0;
                  const rate = h + m > 0 ? (h / (h + m)) * 100 : null;
                  return (
                    <span
                      key={r.replica}
                      style={{
                        font: "400 11px var(--mono)",
                        color: "var(--fg-3)",
                        background: "var(--panel-2)",
                        border: "1px solid var(--line-mid)",
                        borderRadius: 7,
                        padding: "5px 9px",
                      }}
                    >
                      {r.replica} {rate === null ? "—" : `${rate.toFixed(0)}%`}
                    </span>
                  );
                })}
              </Row>
            </div>
          )}
        </Card>

        <Card
          title="Errors by backend"
          subtitle="cumulative since each replica started · retried in-chain before any byte reached a client"
        >
          {worstBackends.length === 0 ? (
            <Empty>No requests recorded yet.</Empty>
          ) : (
            <Table cols={ERR_COLS}>
              {worstBackends.map((b) => (
                <Tr
                  key={b.key}
                  cols={ERR_COLS}
                  cells={[
                    <Row key="n" gap={8} style={{ flexWrap: "nowrap", minWidth: 0 }}>
                      <Dot tone={b.split ? "warn" : b.healthy ? "ok" : "bad"} />
                      <Ellipsis title={b.api_base} style={{ font: "400 12px var(--mono)" }}>
                        {b.model}
                      </Ellipsis>
                    </Row>,
                    <Mono key="h" style={{ color: "var(--fg-4)", font: "400 11px var(--mono)" }}>
                      {hostOf(b.api_base)}
                    </Mono>,
                    <Mono key="r" style={{ font: "400 12px var(--mono)", color: "var(--fg-2)" }}>
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
                    <Row key="b" gap={8} style={{ flexWrap: "nowrap" }}>
                      <Bar
                        pct={Math.min(100, b.errorRate * 100 * 20)}
                        height={5}
                        tone={b.errorRate > 0.01 ? "warn" : "ok"}
                      />
                      <Mono style={{ font: "400 11px var(--mono)", color: "var(--fg-3)" }}>
                        {(b.errorRate * 100).toFixed(2)}%
                      </Mono>
                    </Row>,
                  ]}
                />
              ))}
            </Table>
          )}
        </Card>
      </Grid>

      {dropped > 0 && (
        <Card tone="warn" title="Usage events dropped">
          <Muted>
            {fmtInt(dropped)} event{dropped === 1 ? "" : "s"} were dropped rather than made to wait.
            A full queue means the control plane is not keeping up; the design's stated trade is to
            drop usage rather than block inference, so budgets are undercounted by that many
            requests until it catches up.
          </Muted>
        </Card>
      )}

      <Card
        title="What this screen cannot show"
        subtitle="and where the answer actually lives"
      >
        <Grid cols={3} gap={12}>
          <Unmeasured
            title="Latency percentiles"
            why="TTFT and duration histograms are per process, on each proxy's own /metrics. Merging them needs the buckets — an average of four p99s is not a p99, and it hides exactly the outlier you are looking for."
            how={'histogram_quantile(0.99, sum by (le)\n  (rate(fastllm_ttft_seconds_bucket[5m])))'}
          />
          <Unmeasured
            title="Counter rates before this page loaded"
            why={
              "The rates above are measured by this page and start empty on every load. " +
              "History does exist now — the chart at the top reads usage_events — but it " +
              "counts requests, tokens and latency, not these process counters, which no " +
              "one stores over time. Scrape /metrics if you want those historically."
            }
            how={'rate(fastllm_requests_total[5m])'}
          />
          <Unmeasured
            title="Per-caller latency"
            why="Per-principal detail is deliberately in usage_events, not in Prometheus: a label with that cardinality is how a metrics endpoint becomes an outage."
            how={"SELECT p.name, percentile_cont(0.95)\n  WITHIN GROUP (ORDER BY u.ttft_ms)\nFROM usage_events u …"}
          />
        </Grid>
        {config && !config.otel_endpoint && (
          <div style={{ marginTop: 12 }}>
            <Muted>
              This process has no OTLP endpoint configured, so there are no traces to correlate
              either. <Mono>--otel-endpoint</Mono> turns them on in an <Mono>otel</Mono> build.
            </Muted>
          </div>
        )}
      </Card>
    </Stack>
  );
}

const ERR_COLS = [
  { label: "BACKEND", width: "1.2fr" },
  { label: "HOST", width: "1.2fr" },
  { label: "REQUESTS", width: ".7fr", align: "right" },
  { label: "ERRORS", width: ".6fr", align: "right" },
  { label: "ERROR RATE", width: "1fr" },
];

function ScopeTab({ active, children, ...rest }) {
  return (
    <button
      style={{
        background: active ? "var(--accent-bg)" : "none",
        border: "none",
        borderRadius: 6,
        color: active ? "var(--fg)" : "var(--fg-3)",
        padding: "6px 11px",
        font: "500 11.5px var(--mono)",
      }}
      {...rest}
    >
      {children}
    </button>
  );
}

function RateCard({ title, sub, value, unit, series, tone, digits = 2 }) {
  return (
    <Card>
      <Row style={{ alignItems: "flex-start", flexWrap: "nowrap", marginBottom: 12 }}>
        <div style={{ minWidth: 0 }}>
          <div style={{ font: "600 12px/1.3 var(--sans)" }}>{title}</div>
          <div style={{ font: "400 10px/1.4 var(--sans)", color: "var(--fg-4)" }}>{sub}</div>
        </div>
        <div style={{ flex: 1 }} />
        <div style={{ textAlign: "right", whiteSpace: "nowrap" }}>
          <div style={{ font: "500 16px/1 var(--mono)" }}>
            {value === null ? "—" : value.toFixed(digits)}
          </div>
          <div style={{ font: "400 10px var(--mono)", color: "var(--fg-4)" }}>{unit}</div>
        </div>
      </Row>
      <Spark values={series} tone={tone} height={80} />
    </Card>
  );
}

function Stat({ label, value, hint }) {
  return (
    <div
      style={{
        display: "flex",
        justifyContent: "space-between",
        alignItems: "baseline",
        paddingBottom: 8,
        borderBottom: "1px solid var(--line-soft)",
      }}
    >
      <span style={{ font: "400 12px var(--sans)", color: "var(--fg-3)" }}>{label}</span>
      <span style={{ font: "400 12px var(--mono)", color: "var(--fg)" }}>
        {value}
        {hint && <span style={{ color: "var(--fg-4)" }}> · {hint}</span>}
      </span>
    </div>
  );
}

/**
 * The same window the Overview carries, on the screen where someone has
 * come specifically to look at rates.
 *
 * Everything else here is measured *by this page* since it loaded — that is
 * what makes it a live view, and it is the right answer to "what is
 * happening now". It is the wrong answer to "was it like this an hour ago",
 * which is the question you have when you arrive at this screen because
 * something looked wrong. That question needs the database.
 *
 * Deliberately not scoped by the replica picker above. `usage_events` records
 * which principal and model a request belonged to, never which replica served
 * it — the row is written by the control plane from a batch, and by then the
 * replica is not part of the fact. Filtering this by replica would silently
 * ignore the selection, so it says so instead of pretending.
 */
function MetricsHistory() {
  const [open, setOpen] = useState(false);
  const range = RANGES.find((r) => r.id === "24h");
  const { points, error } = useTimeseries({ range });

  const series = [
    { key: "requests_ok", label: "served", tone: "accent" },
    { key: "upstream_errors", label: "upstream errors", tone: "bad" },
    { key: "refused", label: "refused", tone: "warn" },
  ];
  const lines = [
    { key: "p50_ms", label: "p50", tone: "ok" },
    { key: "p95_ms", label: "p95", tone: "violet" },
  ];

  const shaped = points?.map((p) => {
    const refused =
      p.refused_authorisation +
      p.refused_rate_limit +
      p.refused_budget +
      p.refused_no_backend +
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

  return (
    <>
      <Card
        title="Last 24 hours"
        right={
          <Row gap={10}>
            <Muted>from usage_events · fleet-wide · click to explore</Muted>
            <Button onClick={() => setOpen(true)}>expand</Button>
          </Row>
        }
      >
        {error ? (
          <NotAvailable why={error}>
            History comes from <Mono>GET /admin/timeseries</Mono>. The live rates below are
            unaffected — they are measured by this page and need no database.
          </NotAvailable>
        ) : (
          <div
            onClick={() => setOpen(true)}
            role="button"
            tabIndex={0}
            onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && setOpen(true)}
            style={{ cursor: "pointer" }}
            aria-label="Open traffic history"
          >
            <TimeChart
              points={shaped}
              series={series}
              lines={lines}
              height={150}
              spanSeconds={range.seconds}
            />
            <div style={{ height: 8 }} />
            <Legend series={series} lines={lines} />
          </div>
        )}
      </Card>
      {open && <TimeseriesModal onClose={() => setOpen(false)} />}
    </>
  );
}
