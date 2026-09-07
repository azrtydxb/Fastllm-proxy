// Telling a replica that is *stuck* from one that is merely *catching up*.
//
// The distinction is a question about time, and specifically not about the
// size of the version gap: versions are only republished when the content
// changed, so the gap between two of them is the control plane's edit history
// and says nothing about any replica's health. What decides it is how long the
// newest snapshot has been available. These cases pin that in both directions.
import assert from "node:assert/strict";
import { fleetSummary, convergenceGrace } from "../src/fleet.js";

const CONFIG = { config_poll_seconds: 5, health_report_interval_seconds: 10 };
const GRACE = 20; // 5 + 10 + 5

// A version is the epoch microsecond the control plane built the snapshot.
const NOW_MS = 1_788_800_000_000;
const publishedAgo = (seconds) => (NOW_MS - seconds * 1000) * 1000;
const on = (replica, version) => ({
  replica,
  snapshot_version: version,
  backends: [],
});

let failures = 0;
const check = (name, fn) => {
  try {
    fn();
    console.log(`  ok  ${name}`);
  } catch (e) {
    failures++;
    console.log(`FAIL  ${name}\n      ${e.message}`);
  }
};

check("grace is poll + report + margin", () => {
  assert.equal(convergenceGrace(CONFIG), GRACE);
});

check("grace falls back to shipped defaults", () => {
  assert.equal(convergenceGrace(undefined), GRACE);
  assert.equal(convergenceGrace({}), GRACE);
});

check("grace follows a slower deployment's own timers", () => {
  assert.equal(
    convergenceGrace({
      config_poll_seconds: 30,
      health_report_interval_seconds: 60,
    }),
    95,
  );
});

// The bug this file exists for. Observed live: two replicas one version apart,
// where the two versions happened to be 23s of edit history apart. The old
// code read 23s as staleness and cried wolf; the newest snapshot was 7s old
// and the trailing replica picked it up on its next poll.
check("a big version gap is not a fault when the snapshot is new", () => {
  const s = fleetSummary(
    [on("a", publishedAgo(7)), on("b", publishedAgo(30))],
    CONFIG,
    NOW_MS,
  );
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, ["b"]);
  // The gap really is far wider than the grace -- that is the point.
  assert.ok(s.snapshotSpread / 1e6 > GRACE);
});

// The mirror image, and the reason gap size cannot be the test either way: a
// one-second gap is still a fault once the newer snapshot has been sitting
// there unclaimed for minutes.
check("a tiny version gap is a fault when the snapshot is old", () => {
  const s = fleetSummary(
    [on("a", publishedAgo(300)), on("b", publishedAgo(301))],
    CONFIG,
    NOW_MS,
  );
  assert.deepEqual(s.laggards, ["b"]);
  assert.deepEqual(s.converging, []);
  assert.ok(s.snapshotSpread / 1e6 < 2);
});

check("the boundary itself is not an alert", () => {
  const s = fleetSummary(
    [on("a", publishedAgo(GRACE)), on("b", publishedAgo(GRACE + 60))],
    CONFIG,
    NOW_MS,
  );
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, ["b"]);
});

check("one second past the boundary is", () => {
  const s = fleetSummary(
    [on("a", publishedAgo(GRACE + 1)), on("b", publishedAgo(GRACE + 60))],
    CONFIG,
    NOW_MS,
  );
  assert.deepEqual(s.laggards, ["b"]);
});

check("a fleet in sync is neither, however old the snapshot", () => {
  const v = publishedAgo(3600);
  const s = fleetSummary([on("a", v), on("b", v)], CONFIG, NOW_MS);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, []);
  assert.equal(s.snapshotSpread, 0);
});

check("every replica behind an old snapshot is named", () => {
  const s = fleetSummary(
    [
      on("a", publishedAgo(120)),
      on("b", publishedAgo(200)),
      on("c", publishedAgo(900)),
    ],
    CONFIG,
    NOW_MS,
  );
  assert.deepEqual(s.laggards, ["b", "c"]);
});

check("no reports means no alert and no crash", () => {
  const s = fleetSummary([], CONFIG, NOW_MS);
  assert.equal(s.snapshotVersion, null);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, []);
});

check("a single replica is never behind itself", () => {
  const s = fleetSummary([on("a", publishedAgo(9000))], CONFIG, NOW_MS);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, []);
});

check("a slower deployment widens its own window", () => {
  const slow = { config_poll_seconds: 60, health_report_interval_seconds: 60 };
  const reports = [on("a", publishedAgo(90)), on("b", publishedAgo(400))];
  assert.deepEqual(fleetSummary(reports, slow, NOW_MS).laggards, []);
  assert.deepEqual(fleetSummary(reports, CONFIG, NOW_MS).laggards, ["b"]);
});

// A control-plane clock ahead of the browser's makes the newest snapshot look
// unpublished. That must fail quiet: a false silence costs one poll, a false
// alarm costs the operator's trust in the banner.
check("a snapshot stamped in the future raises nothing", () => {
  const s = fleetSummary(
    [on("a", (NOW_MS + 60_000) * 1000), on("b", publishedAgo(600))],
    CONFIG,
    NOW_MS,
  );
  assert.deepEqual(s.laggards, []);
});

console.log(failures ? `\n${failures} failed` : "\nfleet: all passed");
process.exit(failures ? 1 : 0);
