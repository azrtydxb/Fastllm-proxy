import React, { useEffect, useState } from "react";
import { api, ApiError } from "../api.js";

export function LimitsAndBudgets({ onUnauthorised }) {
  const [limits, setLimits] = useState(null);
  const [budgets, setBudgets] = useState(null);
  const [principals, setPrincipals] = useState([]);
  const [error, setError] = useState(null);

  const load = () =>
    Promise.all([api.get("/admin/limits"), api.get("/admin/budgets"), api.get("/admin/principals")])
      .then(([limits, budgets, principals]) => {
        setLimits(limits);
        setBudgets(budgets);
        setPrincipals(principals);
      })
      .catch((e) => {
        if (e instanceof ApiError && e.status === 401) return onUnauthorised();
        setError(e.message);
      });

  useEffect(load, []);

  return (
    <div>
      <h2>Limits & budgets</h2>
      {error && <div className="error">{error}</div>}

      <h3>Rate limits</h3>
      <LimitForm principals={principals} onSaved={load} setError={setError} />
      {limits === null ? (
        <p className="muted">Loading…</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>principal</th>
              <th>requests/min</th>
              <th>tokens/min</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {limits.map((l) => (
              <tr key={l.principal_id}>
                <td>{l.principal}</td>
                <td>{l.requests_per_min ?? "—"}</td>
                <td>{l.tokens_per_min ?? "—"}</td>
                <td>
                  <button
                    className="danger"
                    onClick={() =>
                      api.del(`/admin/principals/${l.principal_id}/limits`).then(load).catch((e) => setError(e.message))
                    }
                  >
                    remove
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h3>Budgets</h3>
      <BudgetForm principals={principals} onSaved={load} setError={setError} />
      {budgets === null ? (
        <p className="muted">Loading…</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>principal</th>
              <th>used / total</th>
              <th>window</th>
              <th>window start</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {budgets.map((b) => {
              const pct = Math.min(100, Math.round((b.tokens_used / b.tokens_total) * 100));
              return (
                <tr key={b.principal_id}>
                  <td>{b.principal}</td>
                  <td>
                    {b.tokens_used.toLocaleString()} / {b.tokens_total.toLocaleString()} ({pct}%)
                  </td>
                  <td>{b.window}</td>
                  <td>{new Date(b.window_start).toLocaleString()}</td>
                  <td>
                    <button
                      className="danger"
                      onClick={() =>
                        api.del(`/admin/principals/${b.principal_id}/budget`).then(load).catch((e) => setError(e.message))
                      }
                    >
                      remove
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}

function LimitForm({ principals, onSaved, setError }) {
  const [principalId, setPrincipalId] = useState("");
  const [rpm, setRpm] = useState("");
  const [tpm, setTpm] = useState("");
  const save = async () => {
    if (!principalId || (!rpm && !tpm)) return;
    try {
      await api.put(`/admin/principals/${principalId}/limits`, {
        requests_per_min: rpm ? Number(rpm) : undefined,
        tokens_per_min: tpm ? Number(tpm) : undefined,
      });
      setRpm("");
      setTpm("");
      onSaved();
    } catch (e) {
      setError(e.message);
    }
  };
  return (
    <div className="panel">
      <div className="row">
        <select value={principalId} onChange={(e) => setPrincipalId(e.target.value)}>
          <option value="">principal…</option>
          {principals.map((p) => (
            <option key={p.id} value={p.id}>{p.name}</option>
          ))}
        </select>
        <input placeholder="requests/min" value={rpm} onChange={(e) => setRpm(e.target.value)} />
        <input placeholder="tokens/min" value={tpm} onChange={(e) => setTpm(e.target.value)} />
        <button className="primary" onClick={save}>Set</button>
      </div>
    </div>
  );
}

function BudgetForm({ principals, onSaved, setError }) {
  const [principalId, setPrincipalId] = useState("");
  const [total, setTotal] = useState("");
  const [window, setWindow] = useState("daily");
  const save = async () => {
    if (!principalId || !total) return;
    try {
      await api.put(`/admin/principals/${principalId}/budget`, {
        tokens_total: Number(total),
        window,
      });
      setTotal("");
      onSaved();
    } catch (e) {
      setError(e.message);
    }
  };
  return (
    <div className="panel">
      <div className="row">
        <select value={principalId} onChange={(e) => setPrincipalId(e.target.value)}>
          <option value="">principal…</option>
          {principals.map((p) => (
            <option key={p.id} value={p.id}>{p.name}</option>
          ))}
        </select>
        <input placeholder="tokens total" value={total} onChange={(e) => setTotal(e.target.value)} />
        <select value={window} onChange={(e) => setWindow(e.target.value)}>
          <option value="daily">daily</option>
          <option value="weekly">weekly</option>
          <option value="monthly">monthly</option>
        </select>
        <button className="primary" onClick={save}>Set</button>
      </div>
    </div>
  );
}
