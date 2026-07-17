use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use noted_crdt::NotedDoc;
use noted_db::docs;
use uuid::Uuid;

use crate::state::AppState;

const MSG_SYNC: u8 = 0;
const SYNC_STEP1: u8 = 0;
const SYNC_STEP2: u8 = 1;
const SYNC_UPDATE: u8 = 2;

#[derive(Debug, PartialEq, Eq)]
pub enum SyncMsg {
    Step1(Vec<u8>),
    Step2(Vec<u8>),
    Update(Vec<u8>),
}

/// Parse a y-protocols sync frame. Returns `None` for anything malformed or
/// unrecognised — network input is untrusted and must never panic the server.
pub fn parse_msg(bytes: &[u8]) -> Option<SyncMsg> {
    if bytes.len() < 2 || bytes[0] != MSG_SYNC {
        return None;
    }
    let payload = bytes[2..].to_vec();
    match bytes[1] {
        SYNC_STEP1 => Some(SyncMsg::Step1(payload)),
        SYNC_STEP2 => Some(SyncMsg::Step2(payload)),
        SYNC_UPDATE => Some(SyncMsg::Update(payload)),
        _ => None,
    }
}

pub fn encode_msg(msg: &SyncMsg) -> Vec<u8> {
    let (subtype, payload) = match msg {
        SyncMsg::Step1(p) => (SYNC_STEP1, p),
        SyncMsg::Step2(p) => (SYNC_STEP2, p),
        SyncMsg::Update(p) => (SYNC_UPDATE, p),
    };
    let mut out = Vec::with_capacity(payload.len() + 2);
    out.push(MSG_SYNC);
    out.push(subtype);
    out.extend_from_slice(payload);
    out
}

pub async fn handler(
    ws: WebSocketUpgrade,
    Path(page_id): Path<Uuid>,
    State(st): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| session(socket, page_id, st))
}

async fn session(socket: WebSocket, page_id: Uuid, st: AppState) {
    let (mut tx, mut rx) = socket.split();

    // Replay the log to reconstruct server state.
    let updates = match docs::load(&st.pool, page_id).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, %page_id, "failed to load doc");
            return;
        }
    };
    let doc = match NotedDoc::from_updates(&updates) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, %page_id, "corrupt doc log");
            return;
        }
    };

    // Server-initiated Step1 so the client sends us anything we lack.
    if tx
        .send(Message::Binary(encode_msg(&SyncMsg::Step1(doc.state_vector())).into()))
        .await
        .is_err()
    {
        return;
    }

    let mut compaction_disabled = false;

    while let Some(Ok(frame)) = rx.next().await {
        let Message::Binary(bytes) = frame else { continue };
        let Some(msg) = parse_msg(&bytes) else {
            tracing::debug!(%page_id, "ignoring malformed sync frame");
            continue;
        };

        match msg {
            SyncMsg::Step1(sv) => {
                let Ok(diff) = doc.diff(&sv) else { continue };
                if tx
                    .send(Message::Binary(encode_msg(&SyncMsg::Step2(diff)).into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            SyncMsg::Step2(update) | SyncMsg::Update(update) => {
                if doc.apply_update(&update).is_err() {
                    tracing::debug!(%page_id, "rejecting undecodable update");
                    continue;
                }
                if let Err(e) = docs::append(&st.pool, page_id, &update).await {
                    tracing::error!(error = %e, %page_id, "failed to persist update");
                    // Persisting failed. The in-memory doc has already accepted this
                    // update, so continuing would leave server state diverged from the
                    // log — the source of truth — with no way to tell the client.
                    // Close the session instead: the client still holds the edit, and
                    // the sync handshake on reconnect will re-send it (CRDT updates are
                    // idempotent). Closing recovers; continuing loses data silently.
                    break;
                }
                // Compact opportunistically once the log grows long. Skip once a
                // compaction has failed this session: the count stays above the
                // threshold on failure, so retrying unconditionally would re-serialise
                // the whole document (encode_full) plus a DELETE+INSERT on every
                // subsequent message with no backoff. A later session will retry.
                if !compaction_disabled {
                    match docs::update_count(&st.pool, page_id).await {
                        Ok(n) if n > docs::COMPACT_THRESHOLD => {
                            if let Err(e) =
                                docs::compact(&st.pool, page_id, &doc.encode_full()).await
                            {
                                tracing::warn!(error = %e, %page_id, "compaction failed; disabling for this session");
                                compaction_disabled = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
