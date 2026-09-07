import React from "react";
import { hostOf } from "../ui.jsx";

// The fleet as a picture: which planes exist, what is in each, and which way
// the arrows actually point.
//
// The tables below this on the Fleet screen are the detail, and they stay --
// but a table cannot show that the control plane is *not* in the request path,
// that a proxy talks to every backend rather than to one, or that an agent is
// what put a backend on the screen in the first place. Those are the three
// things operators get wrong about this system, and all three are structural.
//
// Two rules this drawing holds to, both of them about not lying:
//
//   - **Every arrow is a real flow with a real interval**, taken from the
//     deployment's own config rather than captioned from memory. The snapshot
//     goes one way and health reports come back the other; drawing that as one
//     double-headed line would suggest a conversation that does not happen.
//   - **Colour is state, never decoration** (`style.css` says so). A box is
//     grey until something is known about it, and a box nobody reports on says
//     so instead of defaulting to green.
//
// Laid out arithmetically rather than measured from the DOM: the geometry is
// simple enough to compute, and a layout that needs a render pass to size
// itself cannot be tested without a browser.

const COL = { control: 0, proxy: 300, host: 640 };
const W = { control: 210, proxy: 250, host: 300 };
const BUS = { snapshot: 268, dispatch: 608 };
const TOP = 68; // below the registration channel
const REG_Y = 26; // the channel agents' registration runs along
const GAP = 11;
const PROXY_H = 92;
const HOST_HEAD = 36;
const HOST_ROW = 18;
const HOST_PAD = 8;

// A model name has to share its row with the counters on the right, and some
// of them are long enough to run straight through them ("nvidia/Qwen3.6-35B-
// A3B-NVFP4" did). Truncating keeps the row readable; the full name is in the
// table below, which is where the detail belongs anyway.
const MODEL_CHARS = 24;
const trunc = (v, n) =>
  String(v).length > n ? `${String(v).slice(0, n - 1)}…` : String(v);

const tone = (t) =>
  t === "ok"
    ? "var(--ok)"
    : t === "warn"
      ? "var(--warn)"
      : t === "bad"
        ? "var(--bad)"
        : "var(--fg-5)";

const bg = (t) =>
  t === "ok"
    ? "var(--ok-bg)"
    : t === "warn"
      ? "var(--warn-bg)"
      : t === "bad"
        ? "var(--bad-bg)"
        : "var(--panel-2)";

const line = (t) =>
  t === "ok"
    ? "var(--ok-line)"
    : t === "warn"
      ? "var(--warn-line)"
      : t === "bad"
        ? "var(--bad-line)"
        : "var(--line)";

/** A replica name is `deploy-hash-suffix`; the suffix is what distinguishes it. */
function shortReplica(name) {
  const parts = String(name).split("-");
  return parts.length > 2 ? parts[parts.length - 1] : name;
}

function fmtUptime(s) {
  if (!Number.isFinite(s)) return "—";
  if (s < 90) return `${Math.round(s)}s`;
  if (s < 5400) return `${Math.round(s / 60)}m`;
  if (s < 172800) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86400)}d`;
}

/**
 * Group what the replicas reported into the hosts those backends live on.
 *
 * The host, not the model, is the unit here: it is what an operator restarts,
 * what an agent registers, and what goes away all at once. `mergeBackends` has
 * already applied the rule that a backend is healthy only if *every* replica
 * says so, so a split shows up as a warning on the host that has it.
 */
export function groupByHost(backends, nodes) {
  const byHost = new Map();
  for (const b of backends) {
    const host = hostOf(b.api_base);
    if (!byHost.has(host))
      byHost.set(host, { host, models: [], node: null, degraded: 0 });
    byHost.get(host).models.push(b);
  }
  // An agent registered these endpoints; attach it to the hosts it owns so the
  // drawing can show who is responsible for the box rather than leaving the
  // agents as an unrelated list further down the page.
  for (const n of nodes || []) {
    for (const api of n.hosts || []) {
      const row = byHost.get(hostOf(api));
      if (row) {
        row.node = n.node;
        row.degraded = n.degraded || 0;
        row.lapsed =
          !n.lease_expires_at || new Date(n.lease_expires_at) < new Date();
      }
    }
  }
  return [...byHost.values()].sort((a, b) => a.host.localeCompare(b.host));
}

/** A host's worst state, since one dead model makes the box not-green. */
function hostTone(row) {
  if (row.models.some((m) => m.split)) return "warn"; // a partition
  if (row.models.every((m) => !m.healthy)) return "bad";
  if (row.models.some((m) => !m.healthy) || row.degraded > 0 || row.lapsed)
    return "warn";
  return "ok";
}

function Box({ x, y, w, h, t, children }) {
  return (
    <g transform={`translate(${x},${y})`}>
      <rect
        width={w}
        height={h}
        rx="8"
        fill={bg(t)}
        stroke={line(t)}
        strokeWidth="1"
      />
      <rect width="3" height={h} rx="1.5" fill={tone(t)} />
      {children}
    </g>
  );
}

const Label = (p) => (
  <text
    fontSize={p.size || 10}
    fontFamily="var(--mono)"
    fill={p.fill || "var(--fg-3)"}
    {...p}
  >
    {p.children}
  </text>
);

/** An arrow with its own label, because an unlabelled arrow states nothing. */
function Flow({
  d,
  label,
  dashed,
  colour = "var(--line-mid)",
  lx,
  ly,
  anchor,
}) {
  return (
    <g>
      <path
        d={d}
        fill="none"
        stroke={colour}
        strokeWidth="1"
        strokeDasharray={dashed ? "3 3" : undefined}
        markerEnd="url(#fleet-arrow)"
      />
      {label && (
        <text
          x={lx}
          y={ly}
          fontSize="9"
          fontFamily="var(--mono)"
          fill="var(--fg-5)"
          textAnchor={anchor || "middle"}
        >
          {label}
        </text>
      )}
    </g>
  );
}

export function FleetMap({ reports, nodes, summary, config, health }) {
  const hosts = groupByHost(summary.backends, nodes);
  const proxies = [...reports].sort((a, b) =>
    a.replica.localeCompare(b.replica),
  );

  const hostH = (r) => HOST_HEAD + r.models.length * HOST_ROW + HOST_PAD;
  const proxyStack = proxies.length * (PROXY_H + GAP) - GAP;
  const hostStack =
    hosts.reduce((a, r) => a + hostH(r) + GAP, 0) - GAP || PROXY_H;
  const bodyH = Math.max(proxyStack, hostStack, 120);
  const H = TOP + bodyH + 24;
  const VW = COL.host + W.host;

  // Vertical centring per column, so two short columns beside a tall one do
  // not sit against the top edge with the arrows raking downwards.
  const proxyTop = TOP + Math.max(0, (bodyH - proxyStack) / 2);
  const hostTop = TOP + Math.max(0, (bodyH - hostStack) / 2);
  const controlH = 108;
  const controlY = TOP + Math.max(0, (bodyH - controlH) / 2);
  const controlMid = controlY + controlH / 2;

  const proxyY = (i) => proxyTop + i * (PROXY_H + GAP);
  const hostY = (i) =>
    hostTop + hosts.slice(0, i).reduce((a, r) => a + hostH(r) + GAP, 0);

  const controlTone = health?.snapshot_rebuild_failures > 0 ? "bad" : "ok";
  const anyAgent = hosts.some((r) => r.node);
  const regRiserMax = hosts.reduce(
    (a, r, i) => (r.node ? Math.max(a, i) : a),
    0,
  );

  return (
    <div style={{ overflowX: "auto" }}>
      <svg
        viewBox={`0 0 ${VW} ${H}`}
        width="100%"
        style={{ minWidth: 720, display: "block" }}
        role="img"
        aria-label="Fleet topology: control plane, proxy replicas, engine hosts"
      >
        <defs>
          <marker
            id="fleet-arrow"
            viewBox="0 0 8 8"
            refX="7"
            refY="4"
            markerWidth="6"
            markerHeight="6"
            orient="auto"
          >
            <path d="M0,1 L7,4 L0,7 z" fill="var(--fg-5)" />
          </marker>
        </defs>

        {/* A band per plane. The left columns hold one box and a handful
            where the right holds every engine host, so without them the
            drawing reads as mostly empty space rather than as three planes of
            very different size -- which is the actual shape of the system. */}
        {[
          [COL.control, W.control],
          [COL.proxy, W.proxy],
          [COL.host, W.host],
        ].map(([x, w]) => (
          <rect
            key={x}
            x={x - 10}
            y={TOP - 22}
            width={w + 20}
            height={bodyH + 32}
            rx="10"
            fill="var(--line-soft)"
            opacity="0.5"
          />
        ))}

        {/* Column headings: the planes, named. */}
        <Label x={COL.control} y={14} size="9" fill="var(--fg-5)">
          MANAGEMENT PLANE
        </Label>
        <Label x={COL.proxy} y={14} size="9" fill="var(--fg-5)">
          DATA PLANE — WORKERS
        </Label>
        <Label x={COL.host} y={14} size="9" fill="var(--fg-5)">
          ENGINE HOSTS
        </Label>

        {/* The control plane. Deliberately not on the request line: it
            publishes configuration and collects reports, and a request never
            waits on it. */}
        <Box
          x={COL.control}
          y={controlY}
          w={W.control}
          h={controlH}
          t={controlTone}
        >
          <Label x="12" y="20" size="11" fill="var(--fg)">
            control plane
          </Label>
          <Label x="12" y="38" fill="var(--fg-4)">
            snapshot {summary.snapshotVersion ? "published" : "none"}
          </Label>
          <Label x="12" y="55" fill="var(--fg-2)">
            {summary.snapshotAgeSeconds > 0
              ? `${Math.round(summary.snapshotAgeSeconds)}s old`
              : "—"}
          </Label>
          <Label x="12" y="74" fill="var(--fg-4)">
            {summary.replicas} replica{summary.replicas === 1 ? "" : "s"}{" "}
            reporting
          </Label>
          <Label
            x="12"
            y="92"
            fill={controlTone === "bad" ? "var(--bad-fg)" : "var(--fg-4)"}
          >
            {health?.snapshot_rebuild_failures
              ? `${health.snapshot_rebuild_failures} rebuild failures`
              : "rebuilds ok"}
          </Label>
        </Box>

        {/* Config down, health back up. Two arrows because they are two
            flows on two timers, not one conversation. */}
        <Flow
          d={`M${COL.control + W.control} ${controlMid - 8} H${BUS.snapshot} V${proxyY(0) + 30} H${COL.proxy}`}
          label={`snapshot · ${config?.config_poll_seconds ?? 5}s`}
          lx={BUS.snapshot - 6}
          ly={controlMid - 14}
          anchor="end"
        />
        <Flow
          d={`M${COL.proxy} ${proxyY(proxies.length - 1) + PROXY_H - 24} H${BUS.snapshot - 12} V${controlMid + 8} H${COL.control + W.control}`}
          dashed
          label={`health · ${config?.health_report_interval_seconds ?? 10}s`}
          lx={BUS.snapshot - 6}
          ly={controlMid + 24}
          anchor="end"
        />

        {proxies.length === 0 && (
          <Label x={COL.proxy} y={TOP + 30} fill="var(--fg-4)">
            no replica reporting
          </Label>
        )}

        {proxies.map((r, i) => {
          const y = proxyY(i);
          const lagging = summary.laggards.includes(r.replica);
          const catching = summary.converging.includes(r.replica);
          const t = lagging ? "bad" : catching ? "warn" : "ok";
          const p = r.process || {};
          const lookups = (p.cache_hits || 0) + (p.cache_misses || 0);
          const inflight = (r.backends || []).reduce(
            (a, b) => a + b.inflight,
            0,
          );
          const unhealthy = (r.backends || []).filter((b) => !b.healthy).length;
          return (
            <React.Fragment key={r.replica}>
              {/* Every worker reaches every backend; drawn to a shared bus
                  rather than as one line per pair, which at four replicas and
                  six hosts is twenty-four lines saying one thing. */}
              <path
                d={`M${COL.proxy + W.proxy} ${y + PROXY_H / 2} H${BUS.dispatch}`}
                stroke="var(--line-mid)"
                strokeWidth="1"
                fill="none"
              />
              <Box x={COL.proxy} y={y} w={W.proxy} h={PROXY_H} t={t}>
                <Label x="12" y="20" size="11" fill="var(--fg)">
                  proxy · {shortReplica(r.replica)}
                </Label>
                <Label x="12" y="37" fill="var(--fg-4)">
                  up {fmtUptime(r.uptime_seconds)} ·{" "}
                  {lagging
                    ? "stuck on an older snapshot"
                    : catching
                      ? "catching up"
                      : "in sync"}
                </Label>
                <Label x="12" y="56" fill="var(--fg-2)">
                  {p.requests_ok ?? 0} ok · {p.requests_failed ?? 0} failed ·{" "}
                  {inflight} in flight
                </Label>
                <Label x="12" y="74" fill="var(--fg-4)">
                  cache{" "}
                  {lookups > 0
                    ? `${Math.round((p.cache_hits / lookups) * 100)}%`
                    : "—"}
                  {unhealthy > 0 ? ` · sees ${unhealthy} backend down` : ""}
                </Label>
              </Box>
            </React.Fragment>
          );
        })}

        {proxies.length > 0 && hosts.length > 0 && (
          <>
            <path
              d={`M${BUS.dispatch} ${proxyY(0) + PROXY_H / 2} V${hostY(hosts.length - 1) + hostH(hosts[hosts.length - 1]) / 2}`}
              stroke="var(--line-mid)"
              strokeWidth="1"
              fill="none"
            />
            <text
              x={BUS.dispatch - 6}
              y={proxyY(0) + PROXY_H / 2 - 8}
              fontSize="9"
              fontFamily="var(--mono)"
              fill="var(--fg-5)"
              textAnchor="end"
            >
              forwards
            </text>
          </>
        )}

        {hosts.length === 0 && (
          <Label x={COL.host} y={TOP + 30} fill="var(--fg-4)">
            no backend reported
          </Label>
        )}

        {hosts.map((row, i) => {
          const y = hostY(i);
          const h = hostH(row);
          const t = hostTone(row);
          return (
            <React.Fragment key={row.host}>
              <path
                d={`M${BUS.dispatch} ${y + h / 2} H${COL.host}`}
                stroke="var(--line-mid)"
                strokeWidth="1"
                fill="none"
                markerEnd="url(#fleet-arrow)"
              />
              <Box x={COL.host} y={y} w={W.host} h={h} t={t}>
                <Label x="12" y="19" size="11" fill="var(--fg)">
                  {row.host}
                </Label>
                <Label
                  x={W.host - 12}
                  y="19"
                  textAnchor="end"
                  fill="var(--fg-4)"
                >
                  {row.node
                    ? `agent ${row.node}${row.lapsed ? " · lapsed" : ""}`
                    : "registered by hand"}
                </Label>
                {row.models.map((m, j) => (
                  <g
                    key={m.model}
                    transform={`translate(12,${HOST_HEAD + j * HOST_ROW})`}
                  >
                    <circle
                      cx="4"
                      cy="-4"
                      r="3.5"
                      fill={tone(m.healthy ? "ok" : "bad")}
                    />
                    <Label x="16" y="0" fill="var(--fg-2)">
                      <title>{m.model}</title>
                      {trunc(m.model, MODEL_CHARS)}
                    </Label>
                    <Label
                      x={W.host - 24}
                      y="0"
                      textAnchor="end"
                      fill="var(--fg-4)"
                    >
                      {m.inflight} in flight · {m.errors}/{m.requests}
                    </Label>
                  </g>
                ))}
              </Box>
            </React.Fragment>
          );
        })}

        {/* Registration, drawn once as a bus. Every agent-owned host gets its
            own riser into a shared channel, and the channel enters the control
            plane a single time -- one path per host would trace the same line
            N times and read as one, with N arrowheads stacked on one point. */}
        {anyAgent && (
          <g>
            {hosts.map((row, i) =>
              row.node ? (
                <path
                  key={row.host}
                  d={`M${COL.host + 30 + i * 16} ${hostY(i)} V${REG_Y}`}
                  stroke="var(--line-mid)"
                  strokeWidth="1"
                  strokeDasharray="3 3"
                  fill="none"
                />
              ) : null,
            )}
            <path
              d={`M${COL.host + 30 + regRiserMax * 16} ${REG_Y} H${COL.control + 40} V${controlY}`}
              stroke="var(--line-mid)"
              strokeWidth="1"
              strokeDasharray="3 3"
              fill="none"
              markerEnd="url(#fleet-arrow)"
            />
            <text
              x={COL.control + 48}
              y={REG_Y - 6}
              fontSize="9"
              fontFamily="var(--mono)"
              fill="var(--fg-5)"
            >
              agents register endpoints on a lease
            </text>
          </g>
        )}
      </svg>
    </div>
  );
}
