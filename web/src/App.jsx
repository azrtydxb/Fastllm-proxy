import React, { useEffect, useState, useCallback } from "react";
import { api, ApiError } from "./api.js";
import { Login } from "./views/Login.jsx";
import { Models } from "./views/Models.jsx";
import { VirtualModels } from "./views/VirtualModels.jsx";
import { Keys } from "./views/Keys.jsx";
import { Principals } from "./views/Principals.jsx";
import { LimitsAndBudgets } from "./views/LimitsAndBudgets.jsx";
import { Usage } from "./views/Usage.jsx";

const TABS = [
  { id: "models", label: "Models", component: Models },
  { id: "virtual-models", label: "Virtual models", component: VirtualModels },
  { id: "keys", label: "Keys", component: Keys },
  { id: "principals", label: "Principals & roles", component: Principals },
  { id: "limits", label: "Limits & budgets", component: LimitsAndBudgets },
  { id: "usage", label: "Usage", component: Usage },
];

// `authed` starts `null` (unknown) rather than `false`, so the login form
// does not flash on screen for a returning user with a still-valid session
// cookie before the first probe request resolves.
export function App() {
  const [authed, setAuthed] = useState(null);
  const [tab, setTab] = useState(TABS[0].id);

  // No `GET /whoami` route exists (nor should one — see the P4 design note
  // about not adding routes the existing admin API can already answer
  // from), so session validity is probed with the cheapest real admin read
  // there is: `GET /admin/health`. A 401 there means "not logged in", which
  // is exactly the question being asked.
  const probe = useCallback(async () => {
    try {
      await api.get("/admin/health");
      setAuthed(true);
    } catch (e) {
      setAuthed(e instanceof ApiError && e.status === 401 ? false : false);
    }
  }, []);

  useEffect(() => {
    probe();
  }, [probe]);

  if (authed === null) {
    return null;
  }
  if (!authed) {
    return <Login onLoggedIn={() => setAuthed(true)} />;
  }

  const logout = async () => {
    try {
      await api.post("/logout");
    } finally {
      setAuthed(false);
    }
  };

  const Active = TABS.find((t) => t.id === tab).component;

  return (
    <div className="app">
      <aside className="sidebar">
        <h1>fastllm-proxy</h1>
        <nav>
          {TABS.map((t) => (
            <button
              key={t.id}
              className={t.id === tab ? "active" : ""}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </nav>
        <div className="who">
          <div>Signed in</div>
          <button onClick={logout}>Log out</button>
        </div>
      </aside>
      <main className="main">
        <Active onUnauthorised={() => setAuthed(false)} />
      </main>
    </div>
  );
}
