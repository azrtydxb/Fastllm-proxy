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
 * Fleet-wide facts that do not belong to any one backend.
 *
 * `config` supplies the convergence grace; `now` is injectable so the
 * classification can be tested without a clock.
 */
export function fleetSummary(reports, config, now = Date.now()) {
  const list = reports || [];
  const versions = list.map((r) => r.snapshot_version);
  const backends = mergeBackends(list);
  const newest = versions.length ? Math.max(...versions) : null;
  const behind =
    newest === null ? [] : list.filter((r) => r.snapshot_version < newest);

  // Whether a behind replica is *stuck* or merely *catching up* is a question
  // about time, and specifically not about the size of the version gap.
  //
  // A version is the wall-clock microsecond at which the control plane built
  // that snapshot, and it only republishes when the content actually changed
  // (`rebuild_once` compares first). So consecutive versions are separated by
  // however long it happened to be between two real changes -- seven seconds
  // here, twenty-three there -- and a replica exactly one version behind shows
  // a "lag" of whatever that gap was. Reading that number as staleness is what
  // made the banner fire: it measured the control plane's edit history, not
  // any replica's health.
  //
  // What does answer the question is how long the newest snapshot has been
  // *available*. Once it has existed for longer than a poll and a report can
  // account for, every replica has had its chance and anyone still behind is
  // genuinely stuck -- however small the version gap. Before that, nobody is
  // late yet, however large it is.
  //
  // Both terms come from the control plane's clock except `now`; skew there
  // only shortens or lengthens the grace slightly, and a clock far enough
  // behind fails quiet rather than crying wolf.
  const availableForSeconds = newest === null ? 0 : (now * 1000 - newest) / 1e6;
  const stuck = availableForSeconds > convergenceGrace(config);

  return {
    replicas: list.length,
    // The check `docs/api.md` recommends, which no single replica can make:
    // a version spread means one is serving an older configuration while
    // answering /health perfectly happily.
    snapshotVersion: newest,
    snapshotSpread: versions.length
      ? Math.max(...versions) - Math.min(...versions)
      : 0,
    // How long the newest snapshot has been published, which is the number
    // the classification below actually turns on.
    snapshotAgeSeconds: availableForSeconds,
    laggards: stuck ? behind.map((r) => r.replica) : [],
    // Behind, but not for long enough to mean anything yet. Named separately
    // so a screen can say "still catching up" instead of either crying wolf or
    // showing an unexplained delay as perfect health.
    converging: stuck ? [] : behind.map((r) => r.replica),
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
