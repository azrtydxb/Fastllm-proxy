// Time-series charts over `GET /admin/timeseries`.
//
// These replace sparklines that were drawn from counters diffed in the
// browser. That approach could only ever show what happened since the page
// loaded: every chart started empty, said "collecting…" for two poll
// intervals, and lost everything on reload. There was no history because
// none was ever asked for.
//
// What changed underneath is that usage is now recorded for every request
// rather than only for principals under a budget, so the database has a
// past to plot. These read it.
//
// Hand-rolled SVG rather than a charting library: the whole need is a line,
// a stack of bars and a hover readout, and the UI has no runtime
// dependencies today. A chart library would be the largest thing in the
// bundle and would bring its own upgrade treadmill for that.

import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { api, query } from "./api.js";
import { Button, Chip, Muted, Row, Spacer } from "./ui.jsx";

const TONE = {
  accent: "var(--accent)",
  ok: "var(--ok)",
  warn: "var(--warn)",
  bad: "var(--bad)",
  violet: "var(--violet)",
};

/** Ranges the drill-down offers, and the bucket each one asks for. */
export const RANGES = [
  { id: "1h", label: "1h", seconds: 3600, bucket: 60 },
  { id: "6h", label: "6h", seconds: 6 * 3600, bucket: 300 },
  { id: "24h", label: "24h", seconds: 24 * 3600, bucket: 900 },
  { id: "7d", label: "7d", seconds: 7 * 86400, bucket: 3600 },
  { id: "30d", label: "30d", seconds: 30 * 86400, bucket: 21600 },
];

/**
 * Fetch a window of history, and keep it fresh.
 *
 * `offset` shifts the window into the past for the pan controls, and is
 * what makes "back" different from "zoom out": the span stays the same, the
 * end moves. Live polling stops when the window is not anchored to now —
 * re-fetching a fixed historical window every few seconds would be pure
 * load for a result that cannot change.
 */
export function useTimeseries({
  range,
  offset = 0,
  model,
  principalId,
  live = true,
}) {
  const [points, setPoints] = useState(null);
  const [error, setError] = useState(null);
  const [tick, setTick] = useState(0);

  const anchored = offset === 0;

  useEffect(() => {
    if (!live || !anchored) return undefined;
    const t = setInterval(() => setTick((n) => n + 1), 5000);
    return () => clearInterval(t);
  }, [live, anchored]);

  useEffect(() => {
    let cancelled = false;
    const until = new Date(Date.now() - offset * range.seconds * 1000);
    const since = new Date(until.getTime() - range.seconds * 1000);
    const q = query({
      since: since.toISOString(),
      until: until.toISOString(),
      bucket: range.bucket,
      model: model || undefined,
      principal_id: principalId || undefined,
    });
    api
      .get(`/admin/timeseries${q}`)
      .then((rows) => {
        if (cancelled) return;
        // Anything that is not an array is not a series, and rendering it
        // would throw inside the chart and — with no error boundary above —
        // take the whole screen down with it. That is not hypothetical: a
        // stale bundle against a control plane missing a route is precisely
        // how five screens went blank here once already. A shape we cannot
        // plot is an error to report, not a value to pass on.
        if (!Array.isArray(rows)) {
          setPoints(null);
          setError("the control plane returned no series for this range");
          return;
        }
        setPoints(rows);
        setError(null);
      })
      .catch((e) => {
        if (!cancelled) setError(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [range.seconds, range.bucket, offset, model, principalId, tick]);

  return { points, error };
}

/** Nice round steps for a y-axis, so gridlines land on readable numbers. */
function niceTicks(max, count = 3) {
  if (!(max > 0)) return [0, 1];
  const raw = max / count;
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const step =
    [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) || mag * 10;
  const top = Math.ceil(max / step) * step;
  const out = [];
  for (let v = 0; v <= top + 1e-9; v += step) out.push(v);
  return out;
}

function fmtTick(v) {
  if (v >= 1e9) return `${(v / 1e9).toFixed(v % 1e9 ? 1 : 0)}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(v % 1e6 ? 1 : 0)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(v % 1e3 ? 1 : 0)}k`;
  return `${Math.round(v * 100) / 100}`;
}

/** Axis labels: a clock for short spans, a date once a day is crossed. */
function fmtAxisTime(iso, spanSeconds) {
  const d = new Date(iso);
  if (spanSeconds <= 6 * 3600)
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (spanSeconds <= 3 * 86400)
    return d.toLocaleString([], { weekday: "short", hour: "2-digit" });
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

/**
 * The chart itself.
 *
 * `series` are stacked bars (counts), `lines` are overlaid lines on their
 * own scale (latency). A bucket with no measurement gives `null`, and the
 * line *breaks* there rather than dropping to the axis — a missing latency
 * and a latency of zero are different facts, and joining across the gap
 * draws a descent to instantaneous that never happened.
 */
export function TimeChart({
  points,
  series = [],
  lines = [],
  height = 130,
  spanSeconds = 3600,
  onHover,
}) {
  const [hover, setHover] = useState(null);
  const ref = useRef(null);
  const box = useRef(null);

  // Render at the container's real pixel width rather than stretching a
  // fixed viewBox to fit.
  //
  // `preserveAspectRatio="none"` scales x and y independently, which is fine
  // for bars and wrong for glyphs: a 600-unit viewBox in a 1500px card
  // stretches every tick label 2.5x horizontally and not at all vertically,
  // so the axis reads as smeared. Measuring means one user unit is one
  // pixel and text is drawn at the size it was asked for.
  const [width, setWidth] = useState(600);
  useEffect(() => {
    const el = box.current;
    if (!el || typeof ResizeObserver === "undefined") return undefined;
    const ro = new ResizeObserver(([entry]) => {
      const w = Math.round(entry.contentRect.width);
      // Guard against 0 during mount and against a resize storm from
      // sub-pixel jitter.
      if (w > 0) setWidth((prev) => (Math.abs(prev - w) > 1 ? w : prev));
    });
    ro.observe(el);
    return () => ro.disconnect();
    // `n` is a dependency because the empty state below returns early with a
    // different element. Without it the observer binds once, to whichever
    // branch rendered first, and a chart that mounted before its data
    // arrived would stay at the fallback width for ever.
  }, [points?.length]);

  const W = width;
  const padL = 34;
  const padR = 8;
  const padT = 8;
  const padB = 16;
  const plotW = W - padL - padR;
  const plotH = height - padT - padB;

  const n = points?.length || 0;
  const stackMax = useMemo(() => {
    if (!n || !series.length) return 0;
    let m = 0;
    for (const p of points) {
      let sum = 0;
      for (const s of series) sum += Number(p[s.key] || 0);
      if (sum > m) m = sum;
    }
    return m;
  }, [points, series, n]);

  const lineMax = useMemo(() => {
    if (!n || !lines.length) return 0;
    let m = 0;
    for (const p of points)
      for (const l of lines) {
        const v = p[l.key];
        if (v != null && v > m) m = v;
      }
    return m;
  }, [points, lines, n]);

  if (!n) {
    return (
      <div
        ref={box}
        style={{
          height,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          font: "400 11px var(--sans)",
          color: "var(--fg-5)",
        }}
      >
        no data in this range
      </div>
    );
  }

  const ticks = niceTicks(stackMax || lineMax);
  const top = ticks[ticks.length - 1] || 1;
  const barW = Math.max(1, (plotW / n) * 0.72);
  const xOf = (i) => padL + ((i + 0.5) / n) * plotW;
  const yOfCount = (v) => padT + plotH - (v / top) * plotH;
  const yOfLine = (v) => padT + plotH - (v / (lineMax || 1)) * plotH;

  const onMove = (e) => {
    const rect = ref.current.getBoundingClientRect();
    const x = ((e.clientX - rect.left) / rect.width) * W;
    const i = Math.round(((x - padL) / plotW) * n - 0.5);
    const clamped = Math.max(0, Math.min(n - 1, i));
    setHover(clamped);
    onHover?.(points[clamped], clamped);
  };
  const onLeave = () => {
    setHover(null);
    onHover?.(null, null);
  };

  // Break the line into runs of consecutive measured buckets, so a gap is a
  // gap rather than a straight segment across it.
  const runsFor = (key) => {
    const runs = [];
    let cur = [];
    points.forEach((p, i) => {
      const v = p[key];
      if (v == null) {
        if (cur.length) runs.push(cur);
        cur = [];
      } else {
        cur.push(`${xOf(i).toFixed(1)},${yOfLine(v).toFixed(1)}`);
      }
    });
    if (cur.length) runs.push(cur);
    return runs;
  };

  return (
    <div ref={box} style={{ position: "relative" }}>
      <svg
        ref={ref}
        viewBox={`0 0 ${W} ${height}`}
        width={W}
        height={height}
        style={{ width: "100%", height, display: "block", cursor: "crosshair" }}
        onMouseMove={onMove}
        onMouseLeave={onLeave}
      >
        {ticks.map((t) => (
          <g key={t}>
            <line
              x1={padL}
              x2={W - padR}
              y1={yOfCount(t)}
              y2={yOfCount(t)}
              stroke="var(--line)"
              strokeWidth="1"
              vectorEffect="non-scaling-stroke"
            />
            <text
              x={padL - 5}
              y={yOfCount(t) + 3}
              textAnchor="end"
              style={{ font: "400 8px var(--mono)", fill: "var(--fg-5)" }}
            >
              {fmtTick(t)}
            </text>
          </g>
        ))}

        {points.map((p, i) => {
          let acc = 0;
          return series.map((s) => {
            const v = Number(p[s.key] || 0);
            if (!v) return null;
            const y0 = yOfCount(acc);
            acc += v;
            const y1 = yOfCount(acc);
            return (
              <rect
                key={`${s.key}-${i}`}
                x={xOf(i) - barW / 2}
                y={y1}
                width={barW}
                height={Math.max(0.5, y0 - y1)}
                fill={TONE[s.tone] || TONE.accent}
                opacity={hover == null || hover === i ? 1 : 0.55}
              />
            );
          });
        })}

        {lines.map((l) =>
          runsFor(l.key).map((run, k) => (
            <polyline
              key={`${l.key}-${k}`}
              points={run.join(" ")}
              fill="none"
              stroke={TONE[l.tone] || TONE.violet}
              strokeWidth="1.5"
              strokeDasharray={l.dashed ? "3 3" : undefined}
              vectorEffect="non-scaling-stroke"
            />
          )),
        )}

        {hover != null && (
          <line
            x1={xOf(hover)}
            x2={xOf(hover)}
            y1={padT}
            y2={padT + plotH}
            stroke="var(--fg-5)"
            strokeWidth="1"
            vectorEffect="non-scaling-stroke"
          />
        )}

        <text
          x={padL}
          y={height - 4}
          style={{ font: "400 8px var(--mono)", fill: "var(--fg-5)" }}
        >
          {fmtAxisTime(points[0].at, spanSeconds)}
        </text>
        <text
          x={W - padR}
          y={height - 4}
          textAnchor="end"
          style={{ font: "400 8px var(--mono)", fill: "var(--fg-5)" }}
        >
          {fmtAxisTime(points[n - 1].at, spanSeconds)}
        </text>
      </svg>

      {hover != null && (
        <ChartTooltip
          point={points[hover]}
          series={series}
          lines={lines}
          left={`${((hover + 0.5) / n) * 100}%`}
        />
      )}
    </div>
  );
}

function ChartTooltip({ point, series, lines, left }) {
  const rows = [
    ...series.map((s) => [
      s.label,
      Number(point[s.key] || 0),
      TONE[s.tone] || TONE.accent,
    ]),
    ...lines
      .filter((l) => point[l.key] != null)
      .map((l) => [l.label, `${point[l.key]} ms`, TONE[l.tone] || TONE.violet]),
  ].filter(([, v]) => v !== 0);

  return (
    <div
      style={{
        position: "absolute",
        top: 0,
        left,
        transform: "translateX(-50%)",
        pointerEvents: "none",
        background: "var(--panel-2)",
        border: "1px solid var(--line-mid)",
        borderRadius: 7,
        padding: "7px 9px",
        minWidth: 130,
        zIndex: 3,
        boxShadow: "0 6px 18px rgba(0,0,0,.35)",
      }}
    >
      <div
        style={{
          font: "500 10px var(--mono)",
          color: "var(--fg-3)",
          marginBottom: 4,
        }}
      >
        {new Date(point.at).toLocaleString([], {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        })}
      </div>
      {rows.length === 0 ? (
        <div style={{ font: "400 11px var(--sans)", color: "var(--fg-5)" }}>
          nothing served
        </div>
      ) : (
        rows.map(([label, value, color]) => (
          <div
            key={label}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 6,
              font: "400 11px var(--sans)",
              color: "var(--fg-2)",
            }}
          >
            <span
              style={{
                width: 7,
                height: 7,
                borderRadius: 2,
                background: color,
                flex: "none",
              }}
            />
            <span style={{ flex: 1 }}>{label}</span>
            <span style={{ font: "500 11px var(--mono)" }}>{value}</span>
          </div>
        ))
      )}
    </div>
  );
}

/** The colour key under a chart. Also the click target's affordance. */
export function Legend({ series = [], lines = [] }) {
  return (
    <Row gap={12} style={{ flexWrap: "wrap" }}>
      {[...series, ...lines].map((s) => (
        <Row key={s.key} gap={5}>
          <span
            style={{
              width: 8,
              height: 8,
              borderRadius: 2,
              background: TONE[s.tone] || TONE.accent,
              flex: "none",
            }}
          />
          <span
            style={{ font: "400 10.5px var(--sans)", color: "var(--fg-4)" }}
          >
            {s.label}
          </span>
        </Row>
      ))}
    </Row>
  );
}
