import React, { useEffect, useState } from "react";
import { api, ApiError } from "../api.js";

// There is no `GET /admin/usage` route, and per the P4 design note this UI
// adds none: raw `usage_events` rows are an accounting detail, not
// something an operator needs to page through, and every number that
// matters from them is already folded into `budgets.tokens_used` by
// `control::api::apply_usage_to_budgets`. This view is a rendering job over
// `/admin/budgets` (consumption) and `/admin/health` (control-plane
// liveness) — the same two routes the other tabs already call.
export function Usage({ onUnauthorised }) {
  const [budgets, setBudgets] = useState(null);
  const [health, setHealth] = useState(null);
  const [error, setError] = useState(null);

  useEffect(() => {
    Promise.all([api.get("/admin/budgets"), api.get("/admin/health")])
      .then(([budgets, health]) => {
        setBudgets(budgets);
        setHealth(health);
      })
      .catch((e) => {
        if (e instanceof ApiError && e.status === 401) return onUnauthorised();
        setError(e.message);
      });
  }, []);

  return (
    <div>
      <h2>Usage</h2>
      {error && <div className="error">{error}</div>}

      <div className="panel">
        <h3>Control plane</h3>
        {health === null ? (
          <p className="muted">Loading…</p>
        ) : (
          <p>
            Snapshot rebuild failures since start:{" "}
            <strong className={health.snapshot_rebuild_failures > 0 ? "error" : "ok"}>
              {health.snapshot_rebuild_failures}
            </strong>
          </p>
        )}
      </div>

      <div className="panel" style={{ maxWidth: 640 }}>
        <h3>Token consumption by principal</h3>
        {budgets === null ? (
          <p className="muted">Loading…</p>
        ) : budgets.length === 0 ? (
          <p className="muted">No principal has a configured budget.</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>principal</th>
                <th>used</th>
                <th>total</th>
                <th>window</th>
              </tr>
            </thead>
            <tbody>
              {budgets.map((b) => (
                <tr key={b.principal_id}>
                  <td>{b.principal}</td>
                  <td>{b.tokens_used.toLocaleString()}</td>
                  <td>{b.tokens_total.toLocaleString()}</td>
                  <td>{b.window}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
