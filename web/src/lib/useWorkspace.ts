"use client";

import { useEffect, useState } from "react";
import { api, UnauthorizedError, type Workspace } from "./api";

/**
 * Which workspace the app is looking at.
 *
 * Replaces `NEXT_PUBLIC_WORKSPACE_ID`. That env var was a single hardcoded
 * uuid, which was fine when there was one workspace and no accounts — and is
 * wrong now that a workspace belongs to whoever is a member of it. A signed-in
 * user's workspaces are a fact about their session, so the app asks the server
 * rather than reading a build-time constant that may name a workspace they
 * cannot open.
 *
 * The env var survives only as a LAST-RESORT fallback for the dev seed data,
 * and is never preferred over what the server says.
 */
const FALLBACK = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

export type WorkspaceState =
  | { status: "loading" }
  | { status: "ready"; current: string; all: Workspace[] }
  | { status: "none" }
  | { status: "failed" };

export function useWorkspace(): WorkspaceState {
  const [state, setState] = useState<WorkspaceState>({ status: "loading" });

  useEffect(() => {
    let cancelled = false;
    api
      .workspaces()
      .then((all) => {
        if (cancelled) return;
        if (all.length > 0) {
          setState({ status: "ready", current: all[0].id, all });
        } else if (FALLBACK) {
          setState({ status: "ready", current: FALLBACK, all: [] });
        } else {
          setState({ status: "none" });
        }
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        if (err instanceof UnauthorizedError) {
          window.location.href = "/signin";
          return;
        }
        setState({ status: "failed" });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return state;
}
