"use client";

import { useCallback, useEffect, useState } from "react";

/**
 * A panel's load is in exactly one of three states.
 *
 * This replaces the older `T | null` convention, where `null` meant BOTH "not
 * loaded yet" and "loaded, and the value was null". A 200 whose body was
 * literally `null` therefore decoded successfully, was stored, and rendered as
 * "Loading…" forever — a hang with no error and nothing to retry. Making
 * "loaded" a distinct tag that carries its data means a loaded value can never
 * be mistaken for the absence of one, whatever the value happens to be.
 */
export type PanelState<T> =
  | { status: "loading" }
  | { status: "ready"; data: T }
  | { status: "failed" };

/**
 * Shared load/retry lifecycle for the dashboard's independently-failing panels.
 *
 * `load` must be referentially stable across renders (wrap it in `useCallback`);
 * it is the effect's dependency, so an inline closure would refetch on every
 * render.
 */
export function usePanelData<T>(load: () => Promise<T>): {
  state: PanelState<T>;
  retry: () => void;
} {
  const [state, setState] = useState<PanelState<T>>({ status: "loading" });
  // Only used to re-trigger the effect on "Try again".
  const [attempt, setAttempt] = useState(0);

  const retry = useCallback(() => {
    setState({ status: "loading" });
    setAttempt((n) => n + 1);
  }, []);

  useEffect(() => {
    let cancelled = false;
    // A retry re-enters here with state already reset to loading; setting it
    // again would be a no-op render, so we only write on settle.
    load()
      .then((data) => {
        if (!cancelled) setState({ status: "ready", data });
      })
      .catch(() => {
        if (!cancelled) setState({ status: "failed" });
      });
    return () => {
      cancelled = true;
    };
  }, [load, attempt]);

  return { state, retry };
}
