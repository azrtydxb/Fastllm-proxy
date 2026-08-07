import React, { useEffect, useState } from "react";
import { api, ApiError } from "../api.js";

export function Principals({ onUnauthorised }) {
  const [principals, setPrincipals] = useState(null);
  const [roles, setRoles] = useState([]);
  const [error, setError] = useState(null);
  const [newName, setNewName] = useState("");
  const [newKind, setNewKind] = useState("service_account");
  const [roleDrafts, setRoleDrafts] = useState({});
  const [passwordDrafts, setPasswordDrafts] = useState({});

  const load = () =>
    Promise.all([api.get("/admin/principals"), api.get("/admin/roles")])
      .then(([principals, roles]) => {
        setPrincipals(principals);
        setRoles(roles);
      })
      .catch((e) => {
        if (e instanceof ApiError && e.status === 401) return onUnauthorised();
        setError(e.message);
      });

  useEffect(load, []);

  const createPrincipal = async (e) => {
    e.preventDefault();
    if (!newName.trim()) return;
    try {
      await api.post("/admin/principals", { name: newName.trim(), kind: newKind });
      setNewName("");
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const deletePrincipal = async (id) => {
    try {
      await api.del(`/admin/principals/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const grantRole = async (id) => {
    const role = roleDrafts[id];
    if (!role) return;
    try {
      await api.post(`/admin/principals/${id}/roles`, { role });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const revokeRole = async (id, role) => {
    try {
      await api.del(`/admin/principals/${id}/roles/${role}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const setPassword = async (id) => {
    const password = passwordDrafts[id];
    if (!password) return;
    try {
      await api.put(`/admin/principals/${id}/password`, { password });
      setPasswordDrafts({ ...passwordDrafts, [id]: "" });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  return (
    <div>
      <h2>Principals & roles</h2>
      {error && <div className="error">{error}</div>}

      <form className="panel" onSubmit={createPrincipal}>
        <h3>New principal</h3>
        <div className="row">
          <input placeholder="name" value={newName} onChange={(e) => setNewName(e.target.value)} />
          <select value={newKind} onChange={(e) => setNewKind(e.target.value)}>
            <option value="service_account">service_account</option>
            <option value="user">user</option>
          </select>
          <button className="primary" type="submit">Create</button>
        </div>
      </form>

      {principals === null ? (
        <p className="muted">Loading…</p>
      ) : (
        principals.map((p) => (
          <div className="panel" key={p.id} style={{ maxWidth: 720 }}>
            <h3>
              {p.name} <span className="pill">{p.kind}</span> <span className="pill">id {p.id}</span>{" "}
              <button className="danger" onClick={() => deletePrincipal(p.id)}>delete</button>
            </h3>
            <div className="row">
              {p.roles.map((r) => (
                <span key={r} className="pill">
                  {r}{" "}
                  <button className="danger" style={{ padding: "0 4px" }} onClick={() => revokeRole(p.id, r)}>
                    ×
                  </button>
                </span>
              ))}
            </div>
            <div className="row">
              <select
                value={roleDrafts[p.id] || ""}
                onChange={(e) => setRoleDrafts({ ...roleDrafts, [p.id]: e.target.value })}
              >
                <option value="">grant role…</option>
                {roles.map((r) => (
                  <option key={r.name} value={r.name}>{r.name}</option>
                ))}
              </select>
              <button className="secondary" onClick={() => grantRole(p.id)}>Grant</button>
            </div>
            {p.kind === "user" && (
              <div className="row">
                <input
                  type="password"
                  placeholder="set login password"
                  value={passwordDrafts[p.id] || ""}
                  onChange={(e) => setPasswordDrafts({ ...passwordDrafts, [p.id]: e.target.value })}
                />
                <button className="secondary" onClick={() => setPassword(p.id)}>Set password</button>
              </div>
            )}
          </div>
        ))
      )}
    </div>
  );
}
