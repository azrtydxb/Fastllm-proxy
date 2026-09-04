// The drill-down behind every chart: a bigger view of the same series, with
// a range to pick, history to walk back through, and filters to narrow it.
//
// The small charts on Overview and Metrics are deliberately fixed to a live
// 24h window — a dashboard tile that also carries controls stops being
// glanceable. Everything adjustable lives here instead, one click away.

import React, { useEffect, useMemo, useState } from "react";
import { api } from "./../api.js";
import { Legend, RANGES, TimeChart, useTimeseries } from "./../charts.jsx";
import {
  Button,
  Chip,
  Empty,
  Label,
  Muted,
  Row,
  Spacer,
  fmtInt,
  fmtMoney,
} from "./../ui.jsx";

const COUNT_SERIES = [
  { key: "requests_ok", label: "served", tone: "accent" },
  { key: "upstream_errors", label: "upstream errors", tone: "bad" },
  {
    key: "refused_authorisation",
    label: "refused: authorisation",
    tone: "warn",
  },
  { key: "refused_rate_limit", label: "refused: rate limit", tone: "warn" },
  { key: "refused_budget", label: "refused: budget", tone: "violet" },
  { key: "refused_no_backend", label: "refused: no backend", tone: "bad" },
  {
    key: "refused_unattributed",
    label: "refused: bad key / unknown model",
    tone: "warn",
  },
];

const LATENCY_LINES = [
  { key: "p50_ms", label: "p50", tone: "ok" },
  { key: "p95_ms", label: "p95", tone: "violet" },
  { key: "ttft_p95_ms", label: "ttft p95", tone: "warn", dashed: true },
];

const TOKEN_SERIES = [
  { key: "prompt_tokens", label: "prompt", tone: "accent" },
  { key: "completion_tokens", label: "completion", tone: "violet" },
];

/**
 * `requests` from the API counts everything *attributable*, refusals
 * included. A stack that plotted it alongside those failures would draw each
 * one twice and always show about double the traffic, so the served band is
 * the remainder after they are taken out.
 *
 * `refused_unattributed` is the exception and is deliberately not subtracted:
 * a 401 or an unknown-model 404 never reached attribution, so it was never
 * inside `requests` to begin with. It rides on top of the stack as its own
 * band, which is also the honest picture — those are requests callers made
 * and saw fail, sitting above the traffic that got far enough to be counted.
 */
function withDerived(points) {
  if (!points) return points;
  return points.map((p) => {
    const attributedFailures =
      p.upstream_errors +
      p.refused_authorisation +
      p.refused_rate_limit +
      p.refused_budget +
      p.refused_no_backend;
    return { ...p, requests_ok: Math.max(0, p.requests - attributedFailures) };
  });
}

export function TimeseriesModal({ initialRange = "24h", onClose }) {
  const [rangeId, setRangeId] = useState(initialRange);
  const [offset, setOffset] = useState(0);
  const [model, setModel] = useState("");
  const [principalId, setPrincipalId] = useState("");
  const [models, setModels] = useState([]);
  const [principals, setPrincipals] = useState([]);

  const range = RANGES.find((r) => r.id === rangeId) || RANGES[2];
  const { points: raw, error } = useTimeseries({
    range,
    offset,
    model,
    principalId,
  });
  const points = useMemo(() => withDerived(raw), [raw]);

  useEffect(() => {
    api
      .get("/admin/provider-models")
      .then(setModels)
      .catch(() => {});
    api
      .get("/admin/principals")
      .then(setPrincipals)
      .catch(() => {});
  }, []);

  // Escape closes, which is the one keyboard affordance a modal owes you.
  useEffect(() => {
    const onKey = (e) => {
      if (e.key === "Escape") onClose();
      if (e.key === "ArrowLeft") setOffset((o) => o + 1);
      if (e.key === "ArrowRight") setOffset((o) => Math.max(0, o - 1));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const totals = useMemo(() => {
    if (!points) return null;
    return points.reduce(
      (a, p) => ({
        requests: a.requests + p.requests + p.refused_unattributed,
        errors: a.errors + p.upstream_errors,
        refused:
          a.refused +
          p.refused_authorisation +
          p.refused_rate_limit +
          p.refused_budget +
          p.refused_no_backend +
          p.refused_unattributed,
        prompt: a.prompt + p.prompt_tokens,
        completion: a.completion + p.completion_tokens,
        cost: a.cost + p.cost_micros,
        unpriced: a.unpriced + p.unpriced_requests,
      }),
      {
        requests: 0,
        errors: 0,
        refused: 0,
        prompt: 0,
        completion: 0,
        cost: 0,
        unpriced: 0,
      },
    );
  }, [points]);

  const windowLabel = offset === 0 ? "live" : `${offset} × ${range.label} ago`;

  return (
    <div
      onClick={onClose}
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,.55)",
        zIndex: 50,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: 24,
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-label="Traffic over time"
        style={{
          background: "var(--panel-2)",
          border: "1px solid var(--line-mid)",
          borderRadius: 12,
          width: "min(1040px, 100%)",
          maxHeight: "100%",
          overflow: "auto",
          padding: 20,
        }}
      >
        <Row gap={10}>
          <div style={{ font: "600 14px var(--sans)" }}>Traffic over time</div>
          <Muted>
            {windowLabel} · {range.label} window · {range.bucket}s buckets
          </Muted>
          <Spacer />
          <Button onClick={onClose}>close</Button>
        </Row>

        <div style={{ height: 14 }} />

        <Row gap={8} style={{ flexWrap: "wrap" }}>
          {RANGES.map((r) => (
            <Chip
              key={r.id}
              active={r.id === rangeId}
              onClick={() => {
                setRangeId(r.id);
                setOffset(0);
              }}
            >
              {r.label}
            </Chip>
          ))}
          <span style={{ width: 8 }} />
          <Button onClick={() => setOffset((o) => o + 1)} title="Older (←)">
            ← older
          </Button>
          <Button
            onClick={() => setOffset((o) => Math.max(0, o - 1))}
            disabled={offset === 0}
            title="Newer (→)"
          >
            newer →
          </Button>
          {offset !== 0 && <Button onClick={() => setOffset(0)}>now</Button>}
          <Spacer />
          <select
            value={model}
            onChange={(e) => setModel(e.target.value)}
            style={selectStyle}
            aria-label="Filter by model"
          >
            <option value="">all models</option>
            {models.map((m) => (
              <option key={m.id} value={m.name}>
                {m.name}
              </option>
            ))}
          </select>
          <select
            value={principalId}
            onChange={(e) => setPrincipalId(e.target.value)}
            style={selectStyle}
            aria-label="Filter by principal"
          >
            <option value="">all principals</option>
            {principals.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </Row>

        {error && (
          <div
            style={{
              marginTop: 12,
              font: "400 12px var(--sans)",
              color: "var(--bad)",
            }}
          >
            {error}
          </div>
        )}

        {totals && (
          <>
            <div style={{ height: 16 }} />
            <Row gap={22} style={{ flexWrap: "wrap" }}>
              <Stat label="REQUESTS" value={fmtInt(totals.requests)} />
              <Stat
                label="UPSTREAM ERRORS"
                value={fmtInt(totals.errors)}
                tone="bad"
              />
              <Stat
                label="REFUSED"
                value={fmtInt(totals.refused)}
                tone="warn"
              />
              <Stat
                label="TOKENS"
                value={fmtInt(totals.prompt + totals.completion)}
                sub={`${fmtInt(totals.prompt)} prompt`}
              />
              <Stat
                label="SPEND"
                value={totals.cost ? fmtMoney(totals.cost) : "—"}
                sub={
                  totals.unpriced
                    ? `${fmtInt(totals.unpriced)} unpriced`
                    : totals.cost
                      ? null
                      : "no priced traffic"
                }
              />
            </Row>
          </>
        )}

        <Section
          title="Requests"
          hint="served, upstream errors and gateway refusals, stacked"
        >
          <TimeChart
            points={points}
            series={COUNT_SERIES}
            height={190}
            spanSeconds={range.seconds}
          />
          <Legend series={COUNT_SERIES} />
        </Section>

        <Section
          title="Latency"
          hint="over the responses a backend returned · a gap is a bucket with nothing to measure, not zero"
        >
          <TimeChart
            points={points}
            lines={LATENCY_LINES}
            height={150}
            spanSeconds={range.seconds}
          />
          <Legend lines={LATENCY_LINES} />
        </Section>

        <Section title="Tokens" hint="only where the response reported counts">
          <TimeChart
            points={points}
            series={TOKEN_SERIES}
            height={150}
            spanSeconds={range.seconds}
          />
          <Legend series={TOKEN_SERIES} />
        </Section>

        {points && points.every((p) => p.requests === 0) && (
          <div style={{ marginTop: 12 }}>
            <Empty>
              Nothing was served in this window. Usage has only been recorded
              for every caller since the accounting change — older windows may
              be empty because nothing was written, not because nothing
              happened.
            </Empty>
          </div>
        )}
      </div>
    </div>
  );
}

const selectStyle = {
  background: "var(--panel)",
  color: "var(--fg-2)",
  border: "1px solid var(--line)",
  borderRadius: 7,
  padding: "6px 8px",
  font: "400 12px var(--sans)",
};

function Stat({ label, value, sub, tone }) {
  return (
    <div>
      <div
        style={{
          font: "500 9.5px var(--sans)",
          color: "var(--fg-5)",
          letterSpacing: ".1em",
        }}
      >
        {label}
      </div>
      <div
        style={{
          font: "500 19px var(--mono)",
          color:
            tone === "bad"
              ? "var(--bad)"
              : tone === "warn"
                ? "var(--warn)"
                : "var(--fg)",
        }}
      >
        {value}
      </div>
      {sub && (
        <div style={{ font: "400 10px var(--sans)", color: "var(--fg-5)" }}>
          {sub}
        </div>
      )}
    </div>
  );
}

function Section({ title, hint, children }) {
  return (
    <div style={{ marginTop: 20 }}>
      <Row gap={8}>
        <div style={{ font: "500 12px var(--sans)" }}>{title}</div>
        <Muted>{hint}</Muted>
      </Row>
      <div style={{ height: 6 }} />
      {children}
    </div>
  );
}
