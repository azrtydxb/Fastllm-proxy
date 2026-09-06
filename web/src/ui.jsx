// The pieces every screen is built from.
//
// Kept in one file rather than a directory of one-component modules because
// they are small, they change together, and a screen author reading this once
// has the whole vocabulary. The rule for adding to it: a component earns a
// place here when a second screen needs it, not in anticipation of one.
//
// # The honesty primitives
//
// `NotAvailable` and `Unmeasured` exist because the gap between what this UI
// wants to show and what the control plane can answer is real, and the wrong
// way to close it is a plausible-looking zero. A backend with no health report
// is not "0 in flight"; a model with no price is not "$0.00". Both of those
// read as facts. These two say what is actually true — the number does not
// exist, and here is why — which is the difference between a dashboard an
// operator can trust and one they learn to second-guess.

import React, { useEffect, useRef, useState } from "react";

// --- layout -----------------------------------------------------------------

export function Card({
  title,
  subtitle,
  right,
  children,
  tone,
  style,
  ...rest
}) {
  const border =
    tone === "warn"
      ? "var(--warn-line)"
      : tone === "bad"
        ? "var(--bad-line)"
        : tone === "accent"
          ? "var(--accent-line)"
          : "var(--line)";
  return (
    <section
      style={{
        background: "var(--panel)",
        border: `1px solid ${border}`,
        borderRadius: "var(--radius)",
        padding: 16,
        minWidth: 0,
        ...style,
      }}
      {...rest}
    >
      {(title || right) && (
        <header
          style={{
            display: "flex",
            alignItems: "flex-start",
            justifyContent: "space-between",
            gap: 12,
            marginBottom: subtitle ? 4 : 14,
          }}
        >
          <div style={{ minWidth: 0 }}>
            {title && (
              <div
                style={{
                  font: "600 13px var(--sans)",
                  letterSpacing: "-.01em",
                }}
              >
                {title}
              </div>
            )}
          </div>
          {right}
        </header>
      )}
      {subtitle && (
        <div
          style={{
            font: "400 11px/1.5 var(--sans)",
            color: "var(--fg-4)",
            marginBottom: 14,
          }}
        >
          {subtitle}
        </div>
      )}
      {children}
    </section>
  );
}

export function Grid({ cols, gap = 14, children, style }) {
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns:
          typeof cols === "number" ? `repeat(${cols},minmax(0,1fr))` : cols,
        gap,
        alignItems: "start",
        ...style,
      }}
    >
      {children}
    </div>
  );
}

export function Stack({ gap = 16, children, style }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap, ...style }}>
      {children}
    </div>
  );
}

export function Row({ gap = 10, align = "center", children, style }) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: align,
        gap,
        flexWrap: "wrap",
        ...style,
      }}
    >
      {children}
    </div>
  );
}

export function Spacer() {
  return <div style={{ flex: 1 }} />;
}

// --- atoms ------------------------------------------------------------------

const TONES = {
  ok: ["var(--ok-bg)", "var(--ok-line)", "var(--ok-fg)"],
  warn: ["var(--warn-bg)", "var(--warn-line)", "var(--warn-fg)"],
  bad: ["var(--bad-bg)", "var(--bad-line)", "var(--bad-fg)"],
  accent: ["var(--accent-bg)", "var(--accent-line)", "var(--accent-soft)"],
  violet: ["#1e1a2b", "#3a3155", "var(--violet-fg)"],
  neutral: ["#151a21", "var(--line)", "var(--fg-2)"],
  quiet: ["#1a1e24", "var(--line)", "var(--fg-3)"],
};

export function Pill({ tone = "neutral", mono, children, style }) {
  const [bg, border, color] = TONES[tone] || TONES.neutral;
  return (
    <span
      style={{
        background: bg,
        border: `1px solid ${border}`,
        borderRadius: 999,
        padding: "2px 8px",
        font: `500 10.5px ${mono ? "var(--mono)" : "var(--sans)"}`,
        color,
        whiteSpace: "nowrap",
        width: "fit-content",
        ...style,
      }}
    >
      {children}
    </span>
  );
}

export function Dot({ tone = "ok", size = 7, style }) {
  const color =
    tone === "ok"
      ? "var(--ok)"
      : tone === "warn"
        ? "var(--warn)"
        : tone === "bad"
          ? "var(--bad)"
          : tone === "accent"
            ? "var(--accent)"
            : "var(--fg-5)";
  return (
    <span
      style={{
        width: size,
        height: size,
        borderRadius: "50%",
        flex: "none",
        background: color,
        ...style,
      }}
    />
  );
}

export function Bar({ pct, tone = "accent", height = 6, track = true }) {
  const color =
    tone === "ok"
      ? "var(--ok)"
      : tone === "warn"
        ? "var(--warn)"
        : tone === "bad"
          ? "var(--bad)"
          : tone === "violet"
            ? "var(--violet)"
            : tone === "muted"
              ? "#4f5661"
              : "var(--accent)";
  return (
    <div
      style={{
        flex: 1,
        height,
        borderRadius: height / 2,
        background: track ? "var(--line-mid)" : "transparent",
        overflow: "hidden",
        minWidth: 24,
      }}
    >
      <div
        style={{
          height: "100%",
          width: `${Math.max(0, Math.min(100, pct))}%`,
          background: color,
          borderRadius: height / 2,
        }}
      />
    </div>
  );
}

export function Label({ children, style }) {
  return (
    <span
      style={{
        font: "500 10px var(--sans)",
        color: "var(--fg-4)",
        letterSpacing: ".08em",
        ...style,
      }}
    >
      {children}
    </span>
  );
}

export function Muted({ children, style }) {
  return (
    <span
      style={{
        font: "400 11px/1.6 var(--sans)",
        color: "var(--fg-4)",
        ...style,
      }}
    >
      {children}
    </span>
  );
}

export function Mono({ children, style }) {
  return (
    <span style={{ fontFamily: "var(--mono)", ...style }}>{children}</span>
  );
}

export function Button({ variant = "ghost", children, style, ...rest }) {
  const base = {
    borderRadius: "var(--radius-sm)",
    padding: "8px 14px",
    font: "500 12px var(--sans)",
    border: "1px solid var(--line)",
    background: "none",
    color: "var(--fg-2)",
  };
  const variants = {
    primary: {
      background: "var(--accent)",
      color: "#06111f",
      border: "none",
      fontWeight: 600,
    },
    secondary: { border: "1px solid #39404b", color: "var(--fg)" },
    ghost: {},
    danger: { border: "1px solid var(--bad-line)", color: "var(--bad-fg)" },
    small: {
      padding: "3px 9px",
      font: "400 11px var(--sans)",
      color: "var(--fg-3)",
    },
    smallDanger: {
      padding: "3px 9px",
      font: "400 11px var(--sans)",
      color: "var(--bad-fg)",
      border: "1px solid var(--bad-line)",
    },
  };
  // A disabled button that looks enabled is a button people click and then
  // wonder about. This applies UI-wide rather than per screen: the prop was
  // being honoured by the DOM and ignored by the styling, so every "Save"
  // waiting on a valid form, and every control mid-request, looked live.
  const state = rest.disabled
    ? { opacity: 0.45, cursor: "not-allowed" }
    : { cursor: "pointer" };
  return (
    <button
      style={{ ...base, ...(variants[variant] || {}), ...state, ...style }}
      {...rest}
    >
      {children}
    </button>
  );
}

export function Chip({ active, children, ...rest }) {
  return (
    <button
      style={{
        background: active ? "var(--accent-bg)" : "var(--panel)",
        border: `1px solid ${active ? "var(--accent-line)" : "var(--line)"}`,
        borderRadius: "var(--radius-sm)",
        color: active ? "var(--fg)" : "var(--fg-3)",
        padding: "7px 12px",
        font: "500 12px var(--sans)",
      }}
      {...rest}
    >
      {children}
    </button>
  );
}

export function Field({ label, hint, children, style }) {
  return (
    <label
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 6,
        minWidth: 0,
        ...style,
      }}
    >
      <Label>{label}</Label>
      {children}
      {hint && (
        <span
          style={{ font: "400 10px/1.5 var(--sans)", color: "var(--fg-5)" }}
        >
          {hint}
        </span>
      )}
    </label>
  );
}

// --- tables -----------------------------------------------------------------

export function Table({ cols, children, style }) {
  return (
    <div style={{ overflowX: "auto", ...style }}>
      <div style={{ minWidth: 0 }}>
        <div
          style={{
            display: "grid",
            gridTemplateColumns: cols.map((c) => c.width || "1fr").join(" "),
            gap: 10,
            padding: "0 0 10px",
            borderBottom: "1px solid var(--line-mid)",
          }}
        >
          {cols.map((c, i) => (
            <Label key={i} style={{ textAlign: c.align || "left" }}>
              {c.label}
            </Label>
          ))}
        </div>
        {children}
      </div>
    </div>
  );
}

export function Tr({ cols, cells, onClick, style }) {
  return (
    <div
      onClick={onClick}
      style={{
        display: "grid",
        gridTemplateColumns: cols.map((c) => c.width || "1fr").join(" "),
        gap: 10,
        alignItems: "center",
        padding: "11px 0",
        borderBottom: "1px solid var(--line-soft)",
        cursor: onClick ? "pointer" : undefined,
        ...style,
      }}
    >
      {cells.map((cell, i) => (
        <div
          key={i}
          style={{
            textAlign: cols[i]?.align || "left",
            minWidth: 0,
            font: "400 12px var(--sans)",
          }}
        >
          {cell}
        </div>
      ))}
    </div>
  );
}

export function Ellipsis({ children, style, title }) {
  return (
    <div
      title={title}
      style={{
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
        minWidth: 0,
        ...style,
      }}
    >
      {children}
    </div>
  );
}

// --- state, honestly --------------------------------------------------------

/** A figure the control plane cannot answer, and the reason it cannot. */
export function NotAvailable({ why, children }) {
  return (
    <span
      title={why}
      style={{
        font: "400 11px var(--sans)",
        color: "var(--fg-5)",
        borderBottom: "1px dotted var(--fg-5)",
        cursor: "help",
      }}
    >
      {children || "not available"}
    </span>
  );
}

/** A whole panel's worth of the same: shown instead of an empty chart. */
export function Unmeasured({ title, why, how }) {
  return (
    <div
      style={{
        border: "1px dashed #2b313a",
        borderRadius: "var(--radius-sm)",
        padding: 16,
        display: "flex",
        flexDirection: "column",
        gap: 6,
      }}
    >
      <div style={{ font: "500 12px var(--sans)", color: "var(--fg-3)" }}>
        {title}
      </div>
      <Muted>{why}</Muted>
      {how && (
        <div
          style={{
            font: "400 11px/1.6 var(--mono)",
            color: "var(--fg-4)",
            background: "var(--panel-2)",
            border: "1px solid var(--line-mid)",
            borderRadius: 7,
            padding: "8px 10px",
            marginTop: 4,
            overflowX: "auto",
          }}
        >
          {how}
        </div>
      )}
    </div>
  );
}

/**
 * The overlay every dialog on this UI sits in.
 *
 * Here rather than per screen because two of them had already grown their own
 * copy of the same fixed-inset backdrop, and the parts worth getting right —
 * a click on the backdrop closing, a click inside not, and Escape working —
 * are exactly the parts a second copy gets subtly wrong.
 */
export function Modal({ label, width = 720, onClose, children }) {
  useEffect(() => {
    const onKey = (e) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

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
        aria-label={label}
        style={{
          background: "var(--panel-2)",
          border: "1px solid var(--line-mid)",
          borderRadius: 12,
          width: `min(${width}px, 100%)`,
          maxHeight: "100%",
          overflow: "auto",
          padding: 20,
        }}
      >
        {children}
      </div>
    </div>
  );
}

/**
 * A name you can click to change.
 *
 * Renaming used to be impossible here, and not by oversight: grants, routing
 * targets and usage history all recorded the name. They are carried by the
 * PATCH route now, so the affordance can exist — but the two names differ in
 * what a rename *means*, which is why each caller passes its own hint rather
 * than this component inventing one.
 */
export function Renamable({ value, hint, style, onSave }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  if (!editing) {
    return (
      <Mono
        style={{ ...style, cursor: "text" }}
        title={hint}
        onClick={() => {
          setDraft(value);
          setEditing(true);
        }}
      >
        {value}
      </Mono>
    );
  }
  return (
    <input
      autoFocus
      value={draft}
      style={{ ...style, font: "500 13px var(--mono)" }}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => setEditing(false)}
      onKeyDown={(e) => {
        if (e.key === "Escape") setEditing(false);
        if (e.key === "Enter" && draft.trim() && draft !== value) {
          setEditing(false);
          onSave(draft.trim());
        }
      }}
    />
  );
}

/**
 * An id, shown short.
 *
 * A uuid is 36 characters and a badge has room for about eight. The first
 * block is what somebody reads out or greps a log for; the whole thing is on
 * the element for a copy or a hover. Truncating is safe *because* it is only
 * ever a label here — every screen addresses a row by the id it already holds,
 * never by one a human retyped.
 */
export function ShortId({ id }) {
  if (!id) return null;
  return (
    <Pill tone="quiet" mono title={id}>
      id {String(id).slice(0, 8)}
    </Pill>
  );
}

export function Empty({ children }) {
  return (
    <div
      style={{
        padding: "18px 0",
        font: "400 12px var(--sans)",
        color: "var(--fg-5)",
      }}
    >
      {children}
    </div>
  );
}

export function Loading() {
  return (
    <div
      style={{
        padding: "18px 0",
        font: "400 12px var(--sans)",
        color: "var(--fg-5)",
        animation: "fastllm-pulse 1.4s ease-in-out infinite",
      }}
    >
      Loading…
    </div>
  );
}

export function ErrorNote({ children, onDismiss }) {
  if (!children) return null;
  return (
    <div
      role="alert"
      style={{
        display: "flex",
        alignItems: "center",
        gap: 12,
        background:
          "linear-gradient(90deg,rgba(229,72,77,.10),rgba(229,72,77,0))",
        border: "1px solid var(--bad-line)",
        borderLeft: "3px solid var(--bad)",
        borderRadius: 10,
        padding: "12px 16px",
        font: "400 12px var(--sans)",
        color: "var(--bad-fg)",
      }}
    >
      <span style={{ minWidth: 0, wordBreak: "break-word" }}>{children}</span>
      <Spacer />
      {onDismiss && (
        <Button variant="small" onClick={onDismiss}>
          dismiss
        </Button>
      )}
    </div>
  );
}

export function Banner({ tone = "warn", children, action }) {
  const [line, fg, glow] =
    tone === "bad"
      ? ["var(--bad-line)", "var(--bad-fg)", "rgba(229,72,77,.10)"]
      : tone === "ok"
        ? ["var(--ok-line)", "var(--ok-fg)", "rgba(47,184,132,.10)"]
        : ["var(--warn-line)", "var(--warn-fg)", "rgba(217,154,43,.10)"];
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 12,
        background: `linear-gradient(90deg,${glow},transparent)`,
        border: `1px solid ${line}`,
        borderLeft: `3px solid ${fg}`,
        borderRadius: 10,
        padding: "12px 16px",
        font: "400 12px/1.5 var(--sans)",
        color: "var(--fg-3)",
      }}
    >
      <span style={{ color: fg, fontWeight: 500 }}>{children}</span>
      <Spacer />
      {action}
    </div>
  );
}

// --- charts -----------------------------------------------------------------

/**
 * A line over values this page collected itself.
 *
 * Deliberately not a general charting component: the only series this UI has
 * are the ones each screen has watched accumulate since it loaded, so
 * there is no range selector, no axis of absolute time, and no way to ask for
 * yesterday. Pretending otherwise is the failure this avoids.
 */
export function Spark({ values, tone = "accent", height = 38, fill = true }) {
  const color =
    tone === "ok"
      ? "var(--ok)"
      : tone === "warn"
        ? "var(--warn)"
        : tone === "bad"
          ? "var(--bad)"
          : tone === "violet"
            ? "var(--violet)"
            : "var(--accent)";
  const rgba =
    tone === "ok"
      ? "rgba(47,184,132,.10)"
      : tone === "warn"
        ? "rgba(217,154,43,.10)"
        : tone === "bad"
          ? "rgba(229,72,77,.10)"
          : tone === "violet"
            ? "rgba(139,123,216,.10)"
            : "rgba(77,148,255,.10)";

  const w = 220;
  const h = height;
  const pad = 4;
  if (!values || values.length < 2) {
    return (
      <div
        style={{
          height,
          display: "flex",
          alignItems: "center",
          font: "400 10px var(--sans)",
          color: "var(--fg-5)",
        }}
      >
        collecting…
      </div>
    );
  }
  const lo = Math.min(...values);
  const hi = Math.max(...values);
  const span = hi - lo || 1;
  const pts = values
    .map((v, i) => {
      const x = (i / (values.length - 1)) * w;
      const y = h - pad - ((v - lo) / span) * (h - pad * 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      style={{ width: "100%", height, display: "block" }}
      aria-hidden="true"
    >
      {fill && (
        <polyline
          points={`0,${h} ${pts} ${w},${h}`}
          fill={rgba}
          stroke="none"
        />
      )}
      <polyline
        points={pts}
        fill="none"
        stroke={color}
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}

export function Donut({ pct, label, sub, tone = "accent" }) {
  const color =
    tone === "ok"
      ? "var(--ok)"
      : tone === "warn"
        ? "var(--warn)"
        : "var(--accent)";
  const turn = Math.max(0, Math.min(1, pct / 100));
  return (
    <div
      style={{
        width: 104,
        height: 104,
        borderRadius: "50%",
        background: `conic-gradient(${color} 0turn ${turn}turn,var(--line-mid) ${turn}turn 1turn)`,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        flex: "none",
      }}
    >
      <div
        style={{
          width: 78,
          height: 78,
          borderRadius: "50%",
          background: "var(--panel)",
          display: "flex",
          flexDirection: "column",
          alignItems: "center",
          justifyContent: "center",
        }}
      >
        <span style={{ font: "500 20px/1 var(--mono)" }}>{label}</span>
        <span
          style={{
            font: "400 9px/1 var(--sans)",
            color: "var(--fg-4)",
            marginTop: 3,
          }}
        >
          {sub}
        </span>
      </div>
    </div>
  );
}

// --- hooks ------------------------------------------------------------------

/**
 * Poll `fn` every `ms`, pausing while the tab is hidden.
 *
 * The first call happens after one interval, not immediately: every caller
 * pairs this with `useLoader`, which already fetches on mount, and firing at
 * once doubled every screen's requests on load.
 */
export function usePoll(fn, ms, deps = []) {
  const saved = useRef(fn);
  saved.current = fn;
  useEffect(() => {
    let alive = true;
    let timer = setTimeout(function tick() {
      if (!alive) return;
      const done = () => {
        if (alive) timer = setTimeout(tick, ms);
      };
      if (document.hidden) done();
      else Promise.resolve(saved.current()).finally(done);
    }, ms);
    return () => {
      alive = false;
      if (timer) clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ms, ...deps]);
}

// --- formatting -------------------------------------------------------------

export function fmtInt(n) {
  if (n === null || n === undefined) return "—";
  return Number(n).toLocaleString();
}

export function fmtCompact(n) {
  if (n === null || n === undefined) return "—";
  const v = Number(n);
  if (Math.abs(v) >= 1e9) return (v / 1e9).toFixed(v >= 1e10 ? 0 : 1) + "B";
  if (Math.abs(v) >= 1e6) return (v / 1e6).toFixed(v >= 1e7 ? 0 : 1) + "M";
  if (Math.abs(v) >= 1e3) return (v / 1e3).toFixed(v >= 1e4 ? 0 : 1) + "k";
  return String(v);
}

/**
 * Money, from the integer micro-units the API speaks.
 *
 * Never rounded up to a whole cent at display time: spend is compared against
 * a cap, and a figure that reads $500.00 when the row says 499.996 makes a
 * budget look breached when it is not.
 */
export function fmtMoney(micros) {
  if (micros === null || micros === undefined) return "—";
  const dollars = Number(micros) / 1e6;
  // Always two decimals, and truncated rather than rounded. Spend is read
  // against a cap: $499.996 shown as "$500" reads as a budget breached that
  // is not, and dropping the decimals above $100 — where the caps actually
  // are — put the rounding exactly where it does the most damage.
  const truncated = Math.trunc(dollars * 100) / 100;
  return (
    "$" +
    truncated.toLocaleString(undefined, {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    })
  );
}

/** Prices are stored per million tokens, in micro-units of currency. */
export function fmtPrice(perMtok) {
  if (perMtok === null || perMtok === undefined) return null;
  return "$" + (Number(perMtok) / 1e6).toFixed(2);
}

export function fmtDuration(seconds) {
  if (seconds === null || seconds === undefined) return "—";
  const s = Math.max(0, Math.floor(seconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d) return `${d}d ${String(h).padStart(2, "0")}h`;
  if (h) return `${h}h ${String(m).padStart(2, "0")}m`;
  if (m) return `${m}m`;
  return `${s}s`;
}

export function fmtBytes(bytes) {
  if (bytes === null || bytes === undefined) return "—";
  const units = ["B", "KiB", "MiB", "GiB"];
  let v = Number(bytes);
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v % 1 === 0 ? v : v.toFixed(1)} ${units[i]}`;
}

/**
 * A snapshot version, made readable.
 *
 * `build_snapshot` stamps `EXTRACT(EPOCH …) * 1000000`, so a version is epoch
 * microseconds: sixteen digits that tell an operator nothing and, worse, make
 * the comparison this UI exists to support — is this replica behind? — an
 * eyeball diff of two long numbers. Rendered as the clock time it actually is,
 * with the raw value kept in a tooltip for anyone matching it against a log.
 */
export function fmtSnapshot(version) {
  if (version === null || version === undefined) return "—";
  const n = Number(version);
  // Anything this large is a microsecond stamp; a small number is a version
  // from some other scheme and is shown as-is rather than mangled into 1970.
  if (n < 1e14) return `v${n}`;
  const d = new Date(n / 1000);
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** How far behind one snapshot is from another, in plain words. */
export function fmtSnapshotLag(behind, ahead) {
  const micros = Number(ahead) - Number(behind);
  if (!Number.isFinite(micros) || micros <= 0) return "";
  // Only a microsecond stamp can be read as elapsed time. A version from some
  // other scheme gets the difference stated plainly rather than a duration
  // computed from units it does not have.
  if (Number(ahead) < 1e14) return `${micros} behind`;
  return fmtDuration(micros / 1e6) + " behind";
}

export function fmtWhen(iso) {
  if (!iso) return "never";
  const t = new Date(iso);
  const diff = (Date.now() - t.getTime()) / 1000;
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)}d ago`;
  return t.toLocaleDateString();
}

export function fmtTime(iso) {
  if (!iso) return "—";
  return new Date(iso).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** The host part of an api_base, which is what identifies a backend on sight. */
export function hostOf(apiBase) {
  try {
    const u = new URL(apiBase);
    return u.host;
  } catch {
    return apiBase;
  }
}
