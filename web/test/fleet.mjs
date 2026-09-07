// The convergence grace, which is the difference between an alert an operator
// trusts and one they learn to skim past.
//
// Every replica polls for a snapshot on its own timer and reports its health
// on another, so a spread of a few seconds after any write is what a healthy
// fleet looks like. These cases pin the boundary in both directions: inside
// the window nothing may be called a laggard, outside it something must be.
import assert from "node:assert/strict";
import { fleetSummary, convergenceGrace } from "../src/fleet.js";

const CONFIG = { config_poll_seconds: 5, health_report_interval_seconds: 10 };
const NEWEST = 1_700_000_000_000_000; // epoch micros
const at = (replica, secondsBehind) => ({
  replica,
  snapshot_version: NEWEST - secondsBehind * 1e6,
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
  assert.equal(convergenceGrace(CONFIG), 20);
});

check("grace falls back to shipped defaults", () => {
  assert.equal(convergenceGrace(undefined), 20);
  assert.equal(convergenceGrace({}), 20);
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

// The bug this file exists for: a 7s spread was reported as a fault, so the
// banner flapped between replicas after every configuration change.
check("a replica inside the window is converging, not a laggard", () => {
  const s = fleetSummary([at("a", 0), at("b", 7)], CONFIG);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, ["b"]);
});

check("a replica past the window is a laggard", () => {
  const s = fleetSummary([at("a", 0), at("b", 240)], CONFIG);
  assert.deepEqual(s.laggards, ["b"]);
  assert.deepEqual(s.converging, []);
});

// Exactly at the boundary counts as converging: the grace is what the timers
// can account for, so the alert belongs strictly beyond it.
check("the boundary itself is not an alert", () => {
  const s = fleetSummary([at("a", 0), at("b", 20)], CONFIG);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, ["b"]);
});

check("one second past the boundary is", () => {
  const s = fleetSummary([at("a", 0), at("b", 21)], CONFIG);
  assert.deepEqual(s.laggards, ["b"]);
});

check("a fleet in sync is neither", () => {
  const s = fleetSummary([at("a", 0), at("b", 0)], CONFIG);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, []);
  assert.equal(s.snapshotSpread, 0);
});

// Both states at once: the slow one must not hide the stuck one, which is the
// failure mode of collapsing this to a single flag.
check("a stuck replica and a converging one are reported apart", () => {
  const s = fleetSummary([at("a", 0), at("b", 6), at("c", 600)], CONFIG);
  assert.deepEqual(s.laggards, ["c"]);
  assert.deepEqual(s.converging, ["b"]);
});

check("no reports means no alert and no crash", () => {
  const s = fleetSummary([], CONFIG);
  assert.equal(s.snapshotVersion, null);
  assert.deepEqual(s.laggards, []);
  assert.deepEqual(s.converging, []);
});

check("a slower deployment widens its own window", () => {
  const slow = { config_poll_seconds: 60, health_report_interval_seconds: 60 };
  assert.deepEqual(fleetSummary([at("a", 0), at("b", 90)], slow).laggards, []);
  assert.deepEqual(fleetSummary([at("a", 0), at("b", 90)], CONFIG).laggards, [
    "b",
  ]);
});

console.log(failures ? `\n${failures} failed` : "\nfleet: all passed");
process.exit(failures ? 1 : 0);
