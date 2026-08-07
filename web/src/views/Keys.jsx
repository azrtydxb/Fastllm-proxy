import React, { useEffect, useState } from "react";
import { api, ApiError } from "../api.js";

export function Keys({ onUnauthorised }) {
  const [keys, setKeys] = useState(null);
  const [principals, setPrincipals] = useState([]);
  const [error, setError] = useState(null);
  const [newName, setNewName] = useState("");
  const [principalId, setPrincipalId] = useState("");
  const [justCreated, setJustCreated] = useState(null);

  const load = () =>
    Promise.all([api.get("/admin/keys"), api.get("/admin/principals")])
      .then(([keys, principals]) => {
        setKeys(keys);
        setPrincipals(principals);
      })
      .catch((e) => {
        if (e instanceof ApiError && e.status === 401) return onUnauthorised();
        setError(e.message);
      });

  useEffect(load, []);

  const createKey = async (e) => {
    e.preventDefault();
    if (!newName.trim() || !principalId) return;
    try {
      const resp = await api.post("/admin/keys", {
        name: newName.trim(),
        principal_id: Number(principalId),
      });
      // Shown once, per the design's explicit requirement — never
      // retrievable again after this response, and this UI does not try.
      setJustCreated(resp);
      setNewName("");
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const revoke = async (id) => {
    try {
      await api.del(`/admin/keys/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  return (
    <div>
      <h2>API keys</h2>
      {error && <div className="error">{error}</div>}

      <form className="panel" onSubmit={createKey}>
        <h3>New key</h3>
        <div className="row">
          <input placeholder="key name" value={newName} onChange={(e) => setNewName(e.target.value)} />
          <select value={principalId} onChange={(e) => setPrincipalId(e.target.value)}>
            <option value="">principal…</option>
            {principals.map((p) => (
              <option key={p.id} value={p.id}>{p.name}</option>
            ))}
          </select>
          <button className="primary" type="submit">Create</button>
        </div>
        {justCreated && (
          <div>
            <div className="ok">Created. Copy this now — it is never shown again:</div>
            <div className="key-plaintext">{justCreated.key}</div>
          </div>
        )}
      </form>

      {keys === null ? (
        <p className="muted">Loading…</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>name</th>
              <th>principal</th>
              <th>prefix</th>
              <th>expires</th>
              <th>status</th>
              <th>last used</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {keys.map((k) => (
              <tr key={k.id}>
                <td>{k.name}</td>
                <td>{k.principal}</td>
                <td><code>{k.prefix}</code></td>
                <td>{k.expires_at ? new Date(k.expires_at).toLocaleDateString() : "never"}</td>
                <td>{k.disabled ? "revoked" : "active"}</td>
                <td>{k.last_used_at ? new Date(k.last_used_at).toLocaleString() : "never"}</td>
                <td>
                  {!k.disabled && (
                    <button className="danger" onClick={() => revoke(k.id)}>revoke</button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
