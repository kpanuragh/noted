import * as Y from "yjs";
import { API_BASE } from "./api";

const MSG_SYNC = 0;
const SYNC_STEP1 = 0;
const SYNC_STEP2 = 1;
const SYNC_UPDATE = 2;

const INITIAL_RECONNECT_DELAY_MS = 500;
const MAX_RECONNECT_DELAY_MS = 15000;

function frame(subtype: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(payload.length + 2);
  out[0] = MSG_SYNC;
  out[1] = subtype;
  out.set(payload, 2);
  return out;
}

/**
 * Minimal y-protocols sync client. y-websocket is not used because the server
 * speaks a hand-rolled subset of the sync protocol (Task 8) and pulling in the
 * full provider would couple us to its room/awareness semantics, which M1a
 * does not have.
 *
 * Reconnection is REQUIRED, not just nice-to-have: the server
 * (`routes/sync.rs`) deliberately closes the socket when persisting an update
 * fails, on the assumption that the client will reconnect and the sync
 * handshake (Step1/Step2) will re-send whatever the server is missing (CRDT
 * updates are idempotent). Without a reconnect here, that server-side close
 * would silently and permanently stop sync for the rest of the session.
 */
export function createProvider(pageId: string, doc: Y.Doc) {
  const url = new URL(`/sync/${pageId}`, API_BASE);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";

  let ws: WebSocket | null = null;
  let destroyed = false;
  let reconnectDelay = INITIAL_RECONNECT_DELAY_MS;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  // Registered once, not per-connection: re-registering on every reconnect
  // would attach a new listener each time and cause each local update to be
  // sent once per accumulated listener.
  const onLocalUpdate = (update: Uint8Array, origin: unknown) => {
    if (origin === "remote") return; // don't echo server updates back
    // If we're disconnected, just skip the send — the Step1/Step2 handshake
    // on the next successful connection re-syncs anything missed.
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(frame(SYNC_UPDATE, update));
    }
  };
  doc.on("update", onLocalUpdate);

  function scheduleReconnect() {
    if (destroyed) return;
    if (reconnectTimer !== null) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      if (destroyed) return;
      reconnectDelay = Math.min(reconnectDelay * 2, MAX_RECONNECT_DELAY_MS);
      connect();
    }, reconnectDelay);
  }

  function connect() {
    if (destroyed) return;
    const socket = new WebSocket(url.toString());
    socket.binaryType = "arraybuffer";
    ws = socket;

    socket.onopen = () => {
      reconnectDelay = INITIAL_RECONNECT_DELAY_MS;
      socket.send(frame(SYNC_STEP1, Y.encodeStateVector(doc)));
    };

    socket.onmessage = (event) => {
      const bytes = new Uint8Array(event.data as ArrayBuffer);
      if (bytes.length < 2 || bytes[0] !== MSG_SYNC) return;
      const payload = bytes.subarray(2);

      switch (bytes[1]) {
        case SYNC_STEP1:
          socket.send(frame(SYNC_STEP2, Y.encodeStateAsUpdate(doc, payload)));
          break;
        case SYNC_STEP2:
        case SYNC_UPDATE:
          Y.applyUpdate(doc, payload, "remote");
          break;
      }
    };

    socket.onclose = () => {
      scheduleReconnect();
    };

    socket.onerror = () => {
      scheduleReconnect();
    };
  }

  connect();

  return {
    destroy() {
      destroyed = true;
      if (reconnectTimer !== null) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      doc.off("update", onLocalUpdate);
      ws?.close();
    },
  };
}
