import { useRef } from "react";

// Merging what the replicas report into what an operator can read.
//
// `GET /admin/fleet` deliberately returns one report per replica and merges
// nothing (see `src/health_report.rs`): the interesting failures are the ones
// where replicas disagree. That leaves the merge to whoever displays it, and
// the merge has rules that are easy to get wrong in ways that make the number
// lie:
//
// - **Counters sum.** Requests, errors and in-flight are per process and add
//   up across the fleet.
// - **Health does not.** A backend is "up" only if every replica that can see
//   it says so. One dissenting replica is a partition — the single most
//   important thing this screen can surface — and majority-voting it away
//   deletes exactly that signal.
// - **The denominator travels with the number.** Every merged figure carries
//   how many replicas contributed, so a fleet total computed from 3 of 4
//   workers says so instead of quietly reading 25% low.

/**
 * Key a backend by what identifies it in the snapshot.
 *
 * Exported so every screen that joins health onto configuration uses this one
 * function. When the views built the same key inline instead, one of them
 * disagreed about the separator and the join silently matched nothing — every
 * health dot grey, every protocol cell a dash, and no error anywhere.
 */
export function backendKey(apiBase, model) {
  return `${apiBase}\u0000${model}`;
}

/**
 * Merge per-replica reports into per-backend rows.
 *
 * Returns rows carrying both the totals and the disagreement, because a UI
 * that shows only the total cannot report a partition.
 */
export function mergeBackends(reports) {
  const byKey = new Map();
  for (const report of reports || []) {
    for (const b of report.backends || []) {
      const key = backendKey(b.api_base, b.model);
      let row = byKey.get(key);
      if (!row) {
        row = {
          key,
          api_base: b.api_base,
          model: b.model,
          inflight: 0,
          requests: 0,
          errors: 0,
          healthyOn: [],
          unhealthyOn: [],
        };
        byKey.set(key, row);
      }
      row.inflight += b.inflight;
      row.requests += b.requests_total;
      row.errors += b.errors_total;
      (b.healthy ? row.healthyOn : row.unhealthyOn).push(report.replica);
    }
  }
  return [...byKey.values()]
    .map((row) => ({
      ...row,
      reporting: row.healthyOn.length + row.unhealthyOn.length,
      // Unanimity, not a majority: see this module's header.
      healthy: row.unhealthyOn.length === 0,
      // The partition signal — some replicas can reach it and some cannot.
      split: row.healthyOn.length > 0 && row.unhealthyOn.length > 0,
      errorRate: row.requests > 0 ? row.errors / row.requests : 0,
    }))
    .sort(
      (a, b) =>
        a.api_base.localeCompare(b.api_base) || a.model.localeCompare(b.model),
    );
}

/**
 * How long the fleet is allowed to take to agree, in seconds.
 *
 * Two independent timers sit between publishing a snapshot and seeing every
 * replica report it: a proxy polls for a new snapshot every
 * `config_poll_seconds`, and reports its health every
 * `health_report_interval_seconds`. A replica that polls just before a new
 * version lands and reports just before its next poll therefore shows the
 * previous version for the sum of the two, with nothing wrong anywhere.
 *
 * The extra margin covers timer drift and the report round trip.
 */
export function convergenceGrace(config) {
  return (
    (config?.config_poll_seconds ?? 5) +
    (config?.health_report_interval_seconds ?? 10) +
    5
  );
}

/**
 * Sort the replicas that are behind into "catching up" and "stuck".
 *
 * The version gap is not the answer, and this is the trap. A version is the
 * microsecond the control plane built that snapshot, and it republishes only
 * when the content actually changed, so two consecutive versions are separated
 * by however long it happened to be between two real changes. A replica
 * exactly one version behind shows a "lag" of whatever that gap was -- seven
 * seconds here, twenty-three there, both healthy. Thresholding it measures the
 * control plane's edit history and calls a converging fleet split.
 *
 * Two signals do answer it, and each covers the other's blind spot:
 *
 * 1. **The newest snapshot is older than the grace.** Then every replica has
 *    had its chance and anyone still behind is stuck -- however small the gap.
 *    This needs no history, so a stuck fleet is named the moment the screen
 *    opens. It goes blind on a busy gateway: `Budget.tokens_used` is part of
 *    the snapshot content (see `Snapshot::content_eq`), so traffic alone
 *    republishes it every few seconds and the newest version is never old.
 *
 * 2. **This replica has been behind continuously for longer than the grace.**
 *    That catches the frozen replica the first signal misses, because being
 *    behind is what persists even while the version it is behind of keeps
 *    moving. It needs history, so it cannot fire until the screen has been
 *    watching for that long -- which is exactly when signal 1 is strongest.
 *
 * `behindSince` carries that history between calls: replica -> the timestamp
 * it was first seen behind, dropped again as soon as it catches up. Pure, with
 * the clock and the map passed in, so both paths are testable without either.
 */
export function classifyLag(reports, config, now, behindSince = {}) {
  const list = reports || [];
  const versions = list.map((r) => r.snapshot_version);
  const newest = versions.length ? Math.max(...versions) : null;
  const grace = convergenceGrace(config);
  const behind =
    newest === null ? [] : list.filter((r) => r.snapshot_version < newest);

  // Only the clock enters here, and only through `now`. Skew against the
  // control plane shortens or lengthens the grace a little; a snapshot stamped
  // in the future fails quiet, because a false silence costs one poll and a
  // false alarm costs trust in the banner.
  const snapshotAgeSeconds = newest === null ? 0 : (now * 1000 - newest) / 1e6;
  const settled = snapshotAgeSeconds > grace;

  const next = {};
  const laggards = [];
  const converging = [];
  for (const r of behind) {
    const since = behindSince[r.replica] ?? now;
    next[r.replica] = since;
    const behindForSeconds = (now - since) / 1000;
    if (settled || behindForSeconds > grace) laggards.push(r.replica);
    else converging.push(r.replica);
  }
  return { laggards, converging, behindSince: next, snapshotAgeSeconds };
}

/**
 * The stateful half of [`classifyLag`], holding the history across polls.
 *
 * The map lives in a ref rather than state: it must not itself trigger a
 * render, and recording "first seen behind" is idempotent, so a double render
 * cannot make a replica look later than it is.
 */
export function useSnapshotLag(reports, config) {
  const behindSince = useRef({});
  const result = classifyLag(reports, config, Date.now(), behindSince.current);
  behindSince.current = result.behindSince;
  return result;
}

/**
 * Fleet-wide facts that do not belong to any one backend.
 *
 * The lag classification here is the history-free half only — right for
 * callers that read counters rather than lag. Screens that show the banner use
 * [`useSnapshotLag`], which also catches a replica frozen while the snapshot
 * keeps moving underneath it.
 */
export function fleetSummary(reports, config, now = Date.now()) {
  const list = reports || [];
  const versions = list.map((r) => r.snapshot_version);
  const backends = mergeBackends(list);
  const lag = classifyLag(list, config, now);
  return {
    replicas: list.length,
    // The check `docs/api.md` recommends, which no single replica can make:
    // a version spread means one is serving an older configuration while
    // answering /health perfectly happily.
    snapshotVersion: versions.length ? Math.max(...versions) : null,
    snapshotSpread: versions.length
      ? Math.max(...versions) - Math.min(...versions)
      : 0,
    snapshotAgeSeconds: lag.snapshotAgeSeconds,
    laggards: lag.laggards,
    converging: lag.converging,
    backends,
    backendsUp: backends.filter((b) => b.healthy).length,
    backendsSplit: backends.filter((b) => b.split),
    requests: backends.reduce((a, b) => a + b.requests, 0),
    errors: backends.reduce((a, b) => a + b.errors, 0),
    inflight: backends.reduce((a, b) => a + b.inflight, 0),
  };
}

/**
 * A rate from two cumulative samples.
 *
 * `null` until there are two, and `null` again if the counter went backwards —
 * which happens when a replica restarts and its totals reset. Reporting the
 * negative jump as a rate would draw a spike of exactly the wrong sign at the
 * moment something actually went wrong.
 */
export function rateBetween(prev, next, seconds) {
  if (!prev || !next || seconds <= 0) return null;
  const delta = next.value - prev.value;
  if (delta < 0) return null;
  return delta / seconds;
}
