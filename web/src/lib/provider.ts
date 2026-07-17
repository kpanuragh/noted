import * as Y from "yjs";
import { API_BASE } from "./api";

const MSG_SYNC = 0;
const SYNC_STEP1 = 0;
const SYNC_STEP2 = 1;
const SYNC_UPDATE = 2;

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
 */
export function createProvider(pageId: string, doc: Y.Doc) {
  const url = new URL(`/sync/${pageId}`, API_BASE);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(url.toString());
  ws.binaryType = "arraybuffer";

  const onLocalUpdate = (update: Uint8Array, origin: unknown) => {
    if (origin === "remote") return; // don't echo server updates back
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(frame(SYNC_UPDATE, update));
    }
  };

  ws.onopen = () => {
    ws.send(frame(SYNC_STEP1, Y.encodeStateVector(doc)));
    doc.on("update", onLocalUpdate);
  };

  ws.onmessage = (event) => {
    const bytes = new Uint8Array(event.data as ArrayBuffer);
    if (bytes.length < 2 || bytes[0] !== MSG_SYNC) return;
    const payload = bytes.subarray(2);

    switch (bytes[1]) {
      case SYNC_STEP1:
        ws.send(frame(SYNC_STEP2, Y.encodeStateAsUpdate(doc, payload)));
        break;
      case SYNC_STEP2:
      case SYNC_UPDATE:
        Y.applyUpdate(doc, payload, "remote");
        break;
    }
  };

  return {
    destroy() {
      doc.off("update", onLocalUpdate);
      ws.close();
    },
  };
}
