import React, { useState } from "react";
import { api } from "../api.js";

export function Login({ onLoggedIn }) {
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.post("/login", { name, password });
      onLoggedIn();
    } catch (err) {
      // `login` deliberately answers the same way for every rejection
      // reason (see control::api::login's doc comment) — this UI does not
      // try to be more specific than the server was willing to be.
      setError("invalid credentials");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login-wrap">
      <form className="panel login-panel" onSubmit={submit}>
        <h3>fastllm-proxy admin</h3>
        <div className="row">
          <input
            placeholder="username"
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoFocus
          />
        </div>
        <div className="row">
          <input
            type="password"
            placeholder="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
        {error && <div className="error">{error}</div>}
        <button className="primary" type="submit" disabled={busy}>
          {busy ? "signing in…" : "sign in"}
        </button>
        <p className="muted" style={{ marginTop: 12 }}>
          No login yet? Bootstrap one from the server with{" "}
          <code>fastllm-proxy set-password --name you --password ...</code>.
        </p>
      </form>
    </div>
  );
}
