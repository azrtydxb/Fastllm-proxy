// Thin fetch wrapper over the admin API. Every screen in this UI is a
// rendering job over routes `src/control/api.rs` exposes; where a screen wants
// something no route can answer, it says so on the page rather than inventing
// it (see `NotAvailable`/`Unmeasured` in ui.jsx).
//
// The session cookie (`fastllm_session`, HttpOnly) is sent automatically by
// the browser on same-origin requests; there is no token for this code to
// hold, which is the whole point of the cookie-session design.

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function request(method, path, body) {
  const resp = await fetch(path, {
    method,
    headers: body ? { "content-type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
    credentials: "same-origin",
  });
  if (resp.status === 401) {
    // The session expired or was never there — bounce to the login screen
    // rather than rendering a broken, half-authenticated view.
    throw new ApiError(401, "unauthenticated");
  }
  if (!resp.ok) {
    let message = `${method} ${path} failed: ${resp.status}`;
    try {
      const parsed = await resp.json();
      if (parsed && parsed.error) message = parsed.error;
    } catch {
      // Body wasn't JSON (e.g. a plain-text 503 from the UI's own
      // not-available fallback) — the generic message above is fine.
    }
    throw new ApiError(resp.status, message);
  }
  if (resp.status === 204) return null;
  const ct = resp.headers.get("content-type") || "";
  return ct.includes("application/json") ? resp.json() : null;
}

/** `?a=1&b=2`, skipping anything unset — an empty filter must not be sent. */
export function query(params) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params || {})) {
    if (v === undefined || v === null || v === "") continue;
    q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : "";
}

export const api = {
  get: (path) => request("GET", path),
  post: (path, body) => request("POST", path, body ?? {}),
  put: (path, body) => request("PUT", path, body ?? {}),
  patch: (path, body) => request("PATCH", path, body ?? {}),
  del: (path, body) => request("DELETE", path, body),
};

export { ApiError };
