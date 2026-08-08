// One loading pattern, so thirteen screens do not each invent their own.
//
// Every screen needs the same four things: fetch on mount, keep the previous
// data visible while refetching, surface an error without blanking the page,
// and hand a 401 back to the shell rather than rendering a half-authenticated
// view. Doing that per screen produced four subtly different behaviours in the
// first version of this UI — including one that cleared the table on every
// poll, which made a 5-second refresh look like a flicker of data loss.

import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "./api.js";

/**
 * Run `fetcher` on mount and on demand.
 *
 * `data` keeps its previous value across a reload, so a poll never blanks the
 * screen; `loading` is only true before the first result arrives.
 */
export function useLoader(fetcher, { onUnauthorised, deps = [] } = {}) {
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [loading, setLoading] = useState(true);
  const alive = useRef(true);
  const saved = useRef(fetcher);
  saved.current = fetcher;

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const reload = useCallback(async () => {
    try {
      const next = await saved.current();
      if (!alive.current) return next;
      setData(next);
      setError(null);
      return next;
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        onUnauthorised?.();
        return null;
      }
      if (alive.current) setError(e.message);
      return null;
    } finally {
      if (alive.current) setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onUnauthorised]);

  useEffect(() => {
    reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return { data, error, loading, reload, setError };
}

/**
 * Wrap a mutation so a failure lands in the screen's error slot.
 *
 * Returns whether it succeeded, which is what a caller needs to decide about
 * clearing a form: clearing on failure loses what the operator typed, and
 * every form here can fail on a duplicate name or a foreign key.
 */
export async function attempt(fn, setError, onUnauthorised) {
  try {
    await fn();
    setError(null);
    return true;
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) {
      onUnauthorised?.();
      return false;
    }
    setError(e.message);
    return false;
  }
}
