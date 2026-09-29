import { useState } from "react";
import {
  Button,
  Field,
  Grid,
  Label,
  Modal,
  Mono,
  Muted,
  Pill,
  Row,
  Stack,
  fmtCompact,
} from "../ui.jsx";

// Flow control for one backend: the admission gate that makes a queue form in
// the proxy, bounded, instead of inside the engine where it is unbounded and
// makes every request slow. See `src/admission.rs` for the mechanism.
//
// Two halves that share this file because they describe the same thing: what
// the gate is set to (on the backend row, edited here) and what it is doing
// (from the fleet's health reports, summed across replicas by `mergeBackends`).

/**
 * What the gate is doing right now, in one line.
 *
 * `live` is a merged fleet row, or undefined when no proxy has reported this
 * backend yet. Amber when the gate is holding traffic back -- the engine's
 * queue drove the ceiling below what was configured, or callers are waiting.
 */
export function FlowStatus({ live, compact = false, alignEnd = false }) {
  const a = live?.admission;
  if (!a) return <Mono style={{ color: "var(--fg-5)" }}>—</Mono>;
  const refused = a.refused_full_total + a.refused_timed_out_total;
  const title =
    `ceiling ${a.capacity} of ${a.max_concurrent} configured, ` +
    `summed over ${a.replicas} replica${a.replicas === 1 ? "" : "s"}\n` +
    `${a.in_use} in use, ${a.queued} waiting at the gate\n` +
    `${fmtCompact(a.admitted_total)} admitted, ` +
    `${fmtCompact(a.refused_full_total)} refused at once (queue full or ` +
    `moved to a sibling), ${fmtCompact(a.refused_timed_out_total)} after waiting`;
  return (
    // A flex row ignores the table cell's text alignment, so a right-aligned
    // column has to ask for it: without this the Fleet page drew the figure
    // hard against the ENGINE column instead of under its own header.
    <Row
      gap={6}
      style={{
        flexWrap: "nowrap",
        justifyContent: alignEnd ? "flex-end" : undefined,
      }}
    >
      <Mono
        title={title}
        style={{
          font: "400 12px var(--mono)",
          color: live.throttled ? "var(--warn-fg)" : "var(--fg-2)",
        }}
      >
        {a.in_use}/{a.capacity}
        {a.capacity < a.max_concurrent ? ` (of ${a.max_concurrent})` : ""}
        {a.queued > 0 ? ` · ${a.queued} waiting` : ""}
      </Mono>
      {!compact && refused > 0 && (
        <Pill tone="warn" mono title={title}>
          {fmtCompact(refused)} refused
        </Pill>
      )}
    </Row>
  );
}

/**
 * The engine's own queue, as the proxies last scraped it. A dash is
 * "unknown" -- a hosted provider, or an engine that publishes no `/metrics`
 * -- and deliberately not 0.
 */
export function EngineQueue({ live }) {
  if (!live || live.engineWaiting === null)
    return <Mono style={{ color: "var(--fg-5)" }}>—</Mono>;
  return (
    <Mono
      title="requests the engine reports running / waiting in its own queue"
      style={{
        font: "400 12px var(--mono)",
        color: live.engineWaiting > 0 ? "var(--warn-fg)" : "var(--fg-3)",
      }}
    >
      {live.engineRunning} run · {live.engineWaiting} wait
    </Mono>
  );
}

/**
 * The flow-control cell on a backend row: live state, and a click to edit.
 */
export function FlowControlCell({ backend, live, onSave }) {
  const [open, setOpen] = useState(false);
  const on = backend.admission_max_concurrent !== null;
  return (
    <>
      <span
        onClick={() => setOpen(true)}
        title="Click to set flow control and the upstream timeout for this backend"
        style={{ cursor: "pointer", minWidth: 0 }}
      >
        {on ? (
          live?.admission ? (
            <FlowStatus live={live} compact />
          ) : (
            <Mono
              style={{ font: "400 12px var(--mono)", color: "var(--fg-3)" }}
            >
              max {backend.admission_max_concurrent}
            </Mono>
          )
        ) : (
          <Mono style={{ font: "400 12px var(--mono)", color: "var(--fg-5)" }}>
            off
          </Mono>
        )}
      </span>
      {open && (
        <FlowControlEditor
          backend={backend}
          live={live}
          onClose={() => setOpen(false)}
          onSave={async (patch) => {
            const ok = await onSave(patch);
            if (ok !== false) setOpen(false);
          }}
        />
      )}
    </>
  );
}

/** A whole number at least `min`, `null` for blank, `NaN` for anything else. */
function wholeOrBlank(raw, min) {
  const t = String(raw ?? "").trim();
  if (t === "") return null;
  const n = Number(t);
  return Number.isInteger(n) && n >= min ? n : NaN;
}

function FlowControlEditor({ backend, live, onClose, onSave }) {
  const [max, setMax] = useState(backend.admission_max_concurrent ?? "");
  const [highWater, setHighWater] = useState(backend.admission_high_water);
  const [queued, setQueued] = useState(backend.admission_max_queued);
  const [wait, setWait] = useState(backend.admission_max_wait_seconds);
  const [timeout, setTimeoutSecs] = useState(
    backend.upstream_timeout_seconds ?? "",
  );
  const [problem, setProblem] = useState(null);

  const save = () => {
    const values = {
      admission_max_concurrent: wholeOrBlank(max, 1),
      admission_high_water: wholeOrBlank(highWater, 1),
      admission_max_queued: wholeOrBlank(queued, 0),
      admission_max_wait_seconds: wholeOrBlank(wait, 0),
      upstream_timeout_seconds: wholeOrBlank(timeout, 1),
    };
    const bad = Object.entries(values).find(([, v]) => Number.isNaN(v));
    if (bad) {
      setProblem(`${bad[0].replaceAll("_", " ")} must be a whole number`);
      return;
    }
    // The three tuning fields always have a value in the database; blank
    // means "leave it", so they are only sent when filled in.
    for (const k of [
      "admission_high_water",
      "admission_max_queued",
      "admission_max_wait_seconds",
    ]) {
      if (values[k] === null) delete values[k];
    }
    onSave(values);
  };

  return (
    <Modal label="Flow control" width={560} onClose={onClose}>
      <Stack gap={14}>
        <Stack gap={4}>
          <Label>Flow control · {backend.provider_name}</Label>
          <Mono style={{ font: "400 12px var(--mono)", color: "var(--fg-3)" }}>
            {backend.api_base} · {backend.upstream_model}
          </Mono>
        </Stack>
        <Muted>
          vLLM accepts every request and queues the surplus itself, which makes
          every request slow instead of failing any. With a gate, each proxy
          sends this engine at most <b>max concurrent</b> requests and holds the
          rest here. While the engine reports <b>high water</b> or more requests
          waiting, the ceiling halves; once its queue drains it grows back by
          one per scrape. Past <b>max queued</b> or <b>max wait</b>, callers get
          503 with Retry-After. Each proxy replica has its own ceiling. Changes
          apply within a snapshot poll.
        </Muted>
        {live?.admission && (
          <Row gap={16}>
            <Label>now</Label>
            <FlowStatus live={live} />
            <EngineQueue live={live} />
          </Row>
        )}
        <Grid cols={2} gap={12}>
          <Field
            label="Max concurrent"
            hint="per proxy replica; blank turns the gate off"
          >
            <input
              value={max}
              placeholder="off"
              onChange={(e) => setMax(e.target.value)}
            />
          </Field>
          <Field
            label="High water"
            hint="engine queue depth that halves the ceiling"
          >
            <input
              value={highWater}
              onChange={(e) => setHighWater(e.target.value)}
            />
          </Field>
          <Field
            label="Max queued"
            hint="callers waiting here before new ones get 503"
          >
            <input value={queued} onChange={(e) => setQueued(e.target.value)} />
          </Field>
          <Field label="Max wait (s)" hint="how long a caller waits for a slot">
            <input value={wait} onChange={(e) => setWait(e.target.value)} />
          </Field>
          <Field
            label="Upstream timeout (s)"
            hint="time to first byte before the request fails; blank uses the deployment default"
          >
            <input
              value={timeout}
              placeholder="default"
              onChange={(e) => setTimeoutSecs(e.target.value)}
            />
          </Field>
        </Grid>
        {problem && <Muted style={{ color: "var(--bad-fg)" }}>{problem}</Muted>}
        <Row gap={8} style={{ justifyContent: "flex-end" }}>
          <Button variant="small" onClick={onClose}>
            cancel
          </Button>
          <Button variant="small" onClick={save}>
            save
          </Button>
        </Row>
      </Stack>
    </Modal>
  );
}
