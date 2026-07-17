use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use noted_crdt::NotedDoc;
use noted_db::docs;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use uuid::Uuid;

use crate::state::{AppState, DocBroadcast, PageHub, next_session_id};

/// How long to wait for edits to stop before rewriting the `blocks` projection.
/// Projecting on every update meant a full DELETE + multi-row INSERT per
/// keystroke; the architecture calls for a debounced projection instead.
const PROJECT_DEBOUNCE: Duration = Duration::from_millis(400);

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

/// Rewrite the `blocks` projection for `page_id` from the hub's document.
///
/// The doc lock is deliberately held across the DB write. Snapshot and write
/// then happen in the same critical section, so concurrent projections cannot
/// land out of order — without this, a session projecting older state could
/// win the race and leave `blocks` stale behind a newer projection.
async fn project_page(hub: &PageHub, st: &AppState, page_id: Uuid) {
    let doc = hub.doc.lock().await;
    let blocks = doc.project();
    if let Err(e) = noted_db::blocks::replace_for_page(&st.pool, page_id, &blocks).await {
        tracing::warn!(error = %e, %page_id, "projection failed");
    }
}

async fn session(socket: WebSocket, page_id: Uuid, st: AppState) {
    let (mut tx, mut rx) = socket.split();

    // One shared document per page, replayed from the log on first attach and
    // reused by every later session on that page.
    let hub = match st.attach_hub(page_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, %page_id, "failed to attach page hub");
            return;
        }
    };
    let session_id = next_session_id();
    let mut bcast = hub.tx.subscribe();

    // Server-initiated Step1 so the client sends us anything we lack.
    let sv = hub.doc.lock().await.state_vector();
    if tx
        .send(Message::Binary(encode_msg(&SyncMsg::Step1(sv)).into()))
        .await
        .is_err()
    {
        st.detach_hub(page_id, &hub).await;
        return;
    }

    let mut compaction_disabled = false;
    // When `Some`, a projection is owed at this instant. Each new update pushes
    // it further out; the deadline branch below is disabled while it is `None`.
    let mut project_at: Option<Instant> = None;

    loop {
        tokio::select! {
            frame = rx.next() => {
                let Some(Ok(frame)) = frame else { break };
                let Message::Binary(bytes) = frame else {
                    continue;
                };
                let Some(msg) = parse_msg(&bytes) else {
                    tracing::debug!(%page_id, "ignoring malformed sync frame");
                    continue;
                };

                match msg {
                    SyncMsg::Step1(sv) => {
                        let diff = { hub.doc.lock().await.diff(&sv) };
                        let Ok(diff) = diff else { continue };
                        if tx
                            .send(Message::Binary(encode_msg(&SyncMsg::Step2(diff)).into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    SyncMsg::Step2(update) | SyncMsg::Update(update) => {
                        // Validate first: never write bytes to the log that we
                        // could not replay back out of it.
                        if NotedDoc::validate_update(&update).is_err() {
                            tracing::debug!(%page_id, "rejecting undecodable update");
                            continue;
                        }

                        // Take the doc lock BEFORE appending, and hold it across
                        // both the append and the apply. That keeps compaction —
                        // which snapshots this doc and then deletes the log it
                        // snapshotted — from interleaving between them and
                        // deleting an update the doc has not absorbed yet.
                        let doc = hub.doc.lock().await;

                        // Persist BEFORE mutating the shared document. The doc is
                        // shared across sessions and outlives this one, so an
                        // update applied here but missing from the log would be
                        // advertised in the doc's state vector forever: the
                        // client's reconnect handshake would conclude the server
                        // already has the edit and never re-send it, losing it
                        // silently. Appending first means a failed write leaves
                        // the shared doc exactly matching the log.
                        if let Err(e) = docs::append(&st.pool, page_id, &update).await {
                            tracing::error!(error = %e, %page_id, "failed to persist update");
                            // Close the session: the client still holds the edit, and
                            // the sync handshake on reconnect re-sends whatever the
                            // server lacks (CRDT updates are idempotent). Closing
                            // recovers; continuing loses data silently.
                            drop(doc);
                            break;
                        }
                        if doc.apply_update(&update).is_err() {
                            // It decoded a moment ago, so this is not malformed
                            // input. The update is safely in the log and a later
                            // replay will pick it up; leave rather than serve a
                            // doc that has silently skipped it.
                            tracing::error!(%page_id, "failed to apply an already-validated update");
                            drop(doc);
                            break;
                        }
                        // Fan the update out to the other tabs on this page. Sent
                        // under the doc lock so broadcast order matches apply order.
                        // An Err here just means nobody else is listening.
                        let _ = hub.tx.send(DocBroadcast {
                            origin: session_id,
                            update: Arc::from(update.as_slice()),
                        });

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
                        drop(doc);

                        // Debounce the projection rather than rewriting every block
                        // on every keystroke. Pushing the deadline out on each update
                        // means a burst of typing projects once, when it settles.
                        project_at = Some(Instant::now() + PROJECT_DEBOUNCE);
                    }
                }
            }

            recv = bcast.recv() => {
                match recv {
                    // Never echo an update back to the client that sent it: that
                    // client already has it, and the tag makes the check exact.
                    Ok(b) if b.origin == session_id => {}
                    Ok(b) => {
                        if tx
                            .send(Message::Binary(
                                encode_msg(&SyncMsg::Update(b.update.to_vec())).into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // This client fell behind and missed frames. Do NOT kill the
                    // session: the log is the source of truth and its next Step1
                    // handshake re-syncs whatever it dropped.
                    Err(RecvError::Lagged(n)) => {
                        tracing::debug!(%page_id, missed = n, "session lagged; will re-sync on next Step1");
                    }
                    // Unreachable while we hold an Arc to the hub that owns the
                    // sender, but recv() would return instantly forever if it did
                    // happen, so leave rather than spin.
                    Err(RecvError::Closed) => break,
                }
            }

            // Disabled while no projection is owed. `unwrap_or` (not `unwrap`)
            // because select! evaluates a branch's expression even when its
            // guard is false — it just never polls the future.
            _ = tokio::time::sleep_until(project_at.unwrap_or_else(Instant::now)),
                if project_at.is_some() =>
            {
                project_at = None;
                project_page(&hub, &st, page_id).await;
            }
        }
    }

    // Project once on the way out so a final edit is never left unprojected —
    // the debounce deadline may not have elapsed when the socket closed.
    //
    // A hard process kill (SIGKILL, crash) can still strand a stale projection:
    // `doc_updates` stays correct because it is written synchronously, but
    // `blocks` may lag. That is exactly what POST /api/pages/{id}/reproject
    // exists to repair.
    if project_at.is_some() {
        project_page(&hub, &st, page_id).await;
    }

    st.detach_hub(page_id, &hub).await;
}
