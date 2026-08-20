import React, { useState } from "react";
import { api } from "../api.js";
import { Button, ErrorNote, Field } from "../ui.jsx";

// The one screen served to an unauthenticated browser.
//
// It names the bootstrap command, because the failure it exists to prevent is
// a fresh install with no password set: the API answers 401 correctly, the
// form rejects every attempt correctly, and an operator with no other signal
// concludes the deployment is broken. `set-password` is the missing step, and
// this is the only place it can be said.

export function Login({ onLoggedIn, error: outerError }) {
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e) => {
    e.preventDefault();
    if (!name || !password || busy) return;
    setBusy(true);
    try {
      const resp = await api.post("/login", { name, password });
      onLoggedIn(resp?.name || name);
    } catch (err) {
      // Deliberately not "no such user" versus "wrong password": the API does
      // not distinguish them either, and a login form that does is a user
      // enumeration oracle.
      setError(
        err.status === 401
          ? "That name and password do not match."
          : err.message,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      style={{
        height: "100%",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        position: "relative",
        overflow: "hidden",
        padding: 24,
      }}
    >
      <div
        style={{
          position: "absolute",
          inset: 0,
          background:
            "radial-gradient(900px 500px at 70% 10%,rgba(77,148,255,.10),transparent 60%)",
        }}
      />
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "minmax(0,1fr) 380px",
          gap: 80,
          maxWidth: 1040,
          width: "100%",
          position: "relative",
          alignItems: "center",
        }}
      >
        <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
          <img
            src="/logo.webp"
            alt="FastLLM Proxy"
            style={{ width: 280, maxWidth: "100%", display: "block" }}
          />
          <div
            style={{
              font: "450 34px/1.2 var(--sans)",
              letterSpacing: "-.02em",
              maxWidth: 460,
              textWrap: "pretty",
            }}
          >
            One endpoint in front of every model you run.
          </div>
          <div
            style={{
              font: "400 14px/1.6 var(--sans)",
              color: "var(--fg-3)",
              maxWidth: 420,
              textWrap: "pretty",
            }}
          >
            Cache-affinity routing, opaque response bodies, RBAC with real keys.
            Sessions are cookie-based; there is no token for the browser to
            hold.
          </div>
        </div>

        <form
          onSubmit={submit}
          style={{
            background: "var(--panel)",
            border: "1px solid var(--line)",
            borderRadius: 14,
            padding: 28,
          }}
        >
          <div style={{ font: "600 15px/1 var(--sans)", marginBottom: 4 }}>
            Sign in
          </div>
          <div
            style={{
              font: "400 12px/1.5 var(--sans)",
              color: "var(--fg-3)",
              marginBottom: 20,
            }}
          >
            Control plane
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
            <Field label="NAME">
              <input
                value={name}
                autoFocus
                autoComplete="username"
                onChange={(e) => setName(e.target.value)}
              />
            </Field>
            <Field label="PASSWORD">
              <input
                type="password"
                value={password}
                autoComplete="current-password"
                onChange={(e) => setPassword(e.target.value)}
              />
            </Field>
            {(error || outerError) && (
              <ErrorNote>{error || outerError}</ErrorNote>
            )}
            <Button
              variant="primary"
              type="submit"
              disabled={busy}
              style={{ marginTop: 6 }}
            >
              {busy ? "Signing in…" : "Sign in"}
            </Button>
          </div>
          <div
            style={{
              marginTop: 18,
              paddingTop: 16,
              borderTop: "1px solid var(--line-mid)",
              font: "400 11px/1.6 var(--sans)",
              color: "var(--fg-4)",
            }}
          >
            No login yet? Bootstrap one on the server:
            <br />
            <span style={{ fontFamily: "var(--mono)", color: "var(--fg-3)" }}>
              fastllm-proxy set-password --name you
            </span>
          </div>
        </form>
      </div>
    </div>
  );
}
