import React, { useEffect, useState } from "react";
import { api, ApiError } from "../api.js";

export function Models({ onUnauthorised }) {
  const [models, setModels] = useState(null);
  const [error, setError] = useState(null);
  const [newName, setNewName] = useState("");
  const [backendDrafts, setBackendDrafts] = useState({});

  const load = () =>
    api
      .get("/admin/models")
      .then(setModels)
      .catch((e) => {
        if (e instanceof ApiError && e.status === 401) return onUnauthorised();
        setError(e.message);
      });

  useEffect(load, []);

  const createModel = async (e) => {
    e.preventDefault();
    if (!newName.trim()) return;
    try {
      await api.post("/admin/models", { name: newName.trim() });
      setNewName("");
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const deleteModel = async (id) => {
    try {
      await api.del(`/admin/models/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const addBackend = async (modelId) => {
    const draft = backendDrafts[modelId] || {};
    if (!draft.api_base) return;
    try {
      await api.post(`/admin/models/${modelId}/backends`, {
        api_base: draft.api_base,
        upstream_model: draft.upstream_model || undefined,
        upstream_api_key: draft.upstream_api_key || undefined,
      });
      setBackendDrafts({ ...backendDrafts, [modelId]: {} });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const deleteBackend = async (backendId) => {
    try {
      await api.del(`/admin/backends/${backendId}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const setDraft = (modelId, patch) =>
    setBackendDrafts({ ...backendDrafts, [modelId]: { ...(backendDrafts[modelId] || {}), ...patch } });

  return (
    <div>
      <h2>Models</h2>
      <p className="muted">
        Per-backend health is tracked by each running proxy process, not by the control plane's
        database — check a running `--role all`/`proxy` instance's own <code>/health</code> for
        live status. This view shows what is configured.
      </p>
      {error && <div className="error">{error}</div>}

      <form className="panel" onSubmit={createModel}>
        <h3>New model</h3>
        <div className="row">
          <input placeholder="model name" value={newName} onChange={(e) => setNewName(e.target.value)} />
          <button className="primary" type="submit">Create</button>
        </div>
      </form>

      {models === null ? (
        <p className="muted">Loading…</p>
      ) : (
        models.map((m) => (
          <div className="panel" key={m.id} style={{ maxWidth: 760 }}>
            <h3>
              {m.name} <span className="pill">id {m.id}</span>{" "}
              <button className="danger" onClick={() => deleteModel(m.id)}>delete model</button>
            </h3>
            <table>
              <thead>
                <tr>
                  <th>api_base</th>
                  <th>upstream_model</th>
                  <th>credential</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {m.backends.map((b) => (
                  <tr key={b.id}>
                    <td>{b.api_base}</td>
                    <td>{b.upstream_model}</td>
                    <td>{b.has_upstream_api_key ? "set" : "—"}</td>
                    <td>
                      <button className="danger" onClick={() => deleteBackend(b.id)}>remove</button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <div className="row">
              <input
                placeholder="api_base"
                value={backendDrafts[m.id]?.api_base || ""}
                onChange={(e) => setDraft(m.id, { api_base: e.target.value })}
              />
              <input
                placeholder="upstream_model (optional)"
                value={backendDrafts[m.id]?.upstream_model || ""}
                onChange={(e) => setDraft(m.id, { upstream_model: e.target.value })}
              />
              <input
                placeholder="api key (optional)"
                type="password"
                value={backendDrafts[m.id]?.upstream_api_key || ""}
                onChange={(e) => setDraft(m.id, { upstream_api_key: e.target.value })}
              />
              <button className="secondary" onClick={() => addBackend(m.id)}>Add backend</button>
            </div>
          </div>
        ))
      )}
    </div>
  );
}
