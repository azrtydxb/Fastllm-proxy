// Thin fetch wrapper over the existing admin API — every view in this UI is
// a rendering job over routes `src/control/api.rs` already exposes, per the
// P4 design note ("driven entirely by the existing admin API — do not add
// new data endpoints"). The session cookie (`fastllm_session`, HttpOnly) is
// sent automatically by the browser on same-origin requests; there is no
// token for this code to manage.

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
      const body = await resp.json();
      if (body && body.error) message = body.error;
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

export const api = {
  get: (path) => request("GET", path),
  post: (path, body) => request("POST", path, body ?? {}),
  put: (path, body) => request("PUT", path, body ?? {}),
  del: (path) => request("DELETE", path),
};

export { ApiError };
