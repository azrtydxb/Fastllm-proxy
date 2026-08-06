import React, { useEffect, useState } from "react";
import { api, ApiError } from "../api.js";

export function VirtualModels({ onUnauthorised }) {
  const [vms, setVms] = useState(null);
  const [models, setModels] = useState([]);
  const [error, setError] = useState(null);
  const [newName, setNewName] = useState("");

  const load = () =>
    Promise.all([api.get("/admin/virtual-models"), api.get("/admin/models")])
      .then(([vms, models]) => {
        setVms(vms);
        setModels(models);
      })
      .catch((e) => {
        if (e instanceof ApiError && e.status === 401) return onUnauthorised();
        setError(e.message);
      });

  useEffect(load, []);

  const createVm = async (e) => {
    e.preventDefault();
    if (!newName.trim()) return;
    try {
      await api.post("/admin/virtual-models", { name: newName.trim() });
      setNewName("");
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const deleteVm = async (id) => {
    try {
      await api.del(`/admin/virtual-models/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const addDefault = async (vmId, modelId) => {
    if (!modelId) return;
    try {
      await api.post(`/admin/virtual-models/${vmId}/defaults`, {
        model_id: Number(modelId),
        weight: 100,
        position: 0,
      });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const removeDefault = async (id) => {
    try {
      await api.del(`/admin/virtual-model-defaults/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const addRule = async (vmId, roles) => {
    try {
      await api.post(`/admin/virtual-models/${vmId}/rules`, {
        position: 0,
        roles: roles ? roles.split(",").map((r) => r.trim()).filter(Boolean) : [],
      });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const removeRule = async (id) => {
    try {
      await api.del(`/admin/rules/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const addTarget = async (ruleId, modelId) => {
    if (!modelId) return;
    try {
      await api.post(`/admin/rules/${ruleId}/targets`, {
        model_id: Number(modelId),
        weight: 100,
        position: 0,
      });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const removeTarget = async (id) => {
    try {
      await api.del(`/admin/rule-targets/${id}`);
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  return (
    <div>
      <h2>Virtual models</h2>
      {error && <div className="error">{error}</div>}

      <form className="panel" onSubmit={createVm}>
        <h3>New virtual model</h3>
        <div className="row">
          <input placeholder="name" value={newName} onChange={(e) => setNewName(e.target.value)} />
          <button className="primary" type="submit">Create</button>
        </div>
      </form>

      {vms === null ? (
        <p className="muted">Loading…</p>
      ) : (
        vms.map((vm) => (
          <div className="panel" key={vm.id} style={{ maxWidth: 820 }}>
            <h3>
              {vm.name} <span className="pill">id {vm.id}</span>{" "}
              <button className="danger" onClick={() => deleteVm(vm.id)}>delete</button>
            </h3>

            <h4 style={{ marginBottom: 4 }}>Rules (evaluated in order; first match wins)</h4>
            {vm.rules.map((rule) => (
              <div key={rule.id} style={{ marginBottom: 8, paddingLeft: 8, borderLeft: "2px solid var(--border)" }}>
                <div className="row">
                  <span className="pill">position {rule.position}</span>
                  <span className="muted">roles: {rule.roles?.join(", ") || "any"}</span>
                  <button className="danger" onClick={() => removeRule(rule.id)}>remove rule</button>
                </div>
                <table>
                  <tbody>
                    {rule.targets.map((t) => (
                      <tr key={t.id}>
                        <td>{t.model}</td>
                        <td>weight {t.weight}</td>
                        <td><button className="danger" onClick={() => removeTarget(t.id)}>remove</button></td>
                      </tr>
                    ))}
                  </tbody>
                </table>
                <AddTarget models={models} onAdd={(modelId) => addTarget(rule.id, modelId)} />
              </div>
            ))}
            <AddRule onAdd={(roles) => addRule(vm.id, roles)} />

            <h4 style={{ marginBottom: 4 }}>Defaults (used when no rule matches)</h4>
            <table>
              <tbody>
                {vm.default_targets.map((t) => (
                  <tr key={t.id}>
                    <td>{t.model}</td>
                    <td>weight {t.weight}</td>
                    <td><button className="danger" onClick={() => removeDefault(t.id)}>remove</button></td>
                  </tr>
                ))}
              </tbody>
            </table>
            <AddTarget models={models} onAdd={(modelId) => addDefault(vm.id, modelId)} />
          </div>
        ))
      )}
    </div>
  );
}

function AddTarget({ models, onAdd }) {
  const [modelId, setModelId] = useState("");
  return (
    <div className="row">
      <select value={modelId} onChange={(e) => setModelId(e.target.value)}>
        <option value="">add target model…</option>
        {models.map((m) => (
          <option key={m.id} value={m.id}>{m.name}</option>
        ))}
      </select>
      <button
        className="secondary"
        onClick={() => {
          onAdd(modelId);
          setModelId("");
        }}
      >
        Add
      </button>
    </div>
  );
}

function AddRule({ onAdd }) {
  const [roles, setRoles] = useState("");
  return (
    <div className="row">
      <input
        placeholder="matching roles, comma separated (blank = any)"
        value={roles}
        onChange={(e) => setRoles(e.target.value)}
      />
      <button
        className="secondary"
        onClick={() => {
          onAdd(roles);
          setRoles("");
        }}
      >
        Add rule
      </button>
    </div>
  );
}
