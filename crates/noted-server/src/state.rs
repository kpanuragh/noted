use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use noted_crdt::NotedDoc;
use noted_db::PgPool;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

/// How many updates a slow session may fall behind before it is marked lagged.
/// A lagged session is NOT killed: it drops the missed frames and re-syncs on
/// its next Step1 handshake, because the CRDT log — not this channel — is the
/// source of truth.
const BROADCAST_CAPACITY: usize = 256;

/// One update, tagged with the session that produced it.
///
/// The tag is what stops a session echoing its own edit back to the client it
/// came from: `broadcast` fans out to every subscriber including the sender's
/// own, so each session skips frames whose `origin` is itself. Comparing update
/// bytes instead would be subtler and would misfire on two clients that
/// legitimately produce identical payloads; an explicit id cannot.
#[derive(Clone)]
pub struct DocBroadcast {
    pub origin: u64,
    pub update: Arc<[u8]>,
}

/// Monotonic source of session ids. Only ever compared for equality, so wrapping
/// after 2^64 sessions is not a concern.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

pub fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// The single shared server-side document for one page, plus the fan-out channel
/// that keeps every session on that page in step.
///
/// Before this existed each WebSocket session replayed its own private
/// `NotedDoc`, so two tabs on one page never saw each other's edits and each
/// one's `blocks::replace_for_page` deleted the other's rows and wrote its own
/// stale view.
pub struct PageHub {
    pub doc: Mutex<NotedDoc>,
    pub tx: broadcast::Sender<DocBroadcast>,
    /// Live sessions attached to this hub. Read and written ONLY while the
    /// registry lock is held — that is what makes "last session out removes the
    /// hub" atomic against a concurrent session attaching.
    sessions: AtomicUsize,
}

impl PageHub {
    fn new(doc: NotedDoc) -> Self {
        Self {
            doc: Mutex::new(doc),
            tx: broadcast::channel(BROADCAST_CAPACITY).0,
            sessions: AtomicUsize::new(0),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// One hub per page that currently has at least one session. Empty pages are
    /// removed on last disconnect so this cannot grow to hold every page ever
    /// opened.
    pub hubs: Arc<Mutex<HashMap<Uuid, Arc<PageHub>>>>,
}

impl AppState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            hubs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach a session to `page_id`'s hub, creating it from the log if this is
    /// the first session on the page. The returned hub has already been counted;
    /// the caller MUST pair this with [`AppState::detach_hub`].
    pub async fn attach_hub(&self, page_id: Uuid) -> Result<Arc<PageHub>, HubError> {
        // Fast path: hub already exists. The registry lock covers a HashMap
        // lookup and nothing else.
        {
            let hubs = self.hubs.lock().await;
            if let Some(hub) = hubs.get(&page_id) {
                hub.sessions.fetch_add(1, Ordering::AcqRel);
                return Ok(Arc::clone(hub));
            }
        }

        // Slow path: replay the log with the registry lock RELEASED. Holding it
        // across a DB round trip would stall connections to every other page.
        let updates = noted_db::docs::load(&self.pool, page_id).await?;
        let doc = NotedDoc::from_updates(&updates).map_err(|e| HubError::Corrupt(e.to_string()))?;

        let mut hubs = self.hubs.lock().await;
        // Another session may have created the hub while we were loading. If so,
        // adopt theirs and drop ours: both replayed the same log, but theirs may
        // already have newer edits applied on top.
        let hub = hubs
            .entry(page_id)
            .or_insert_with(|| Arc::new(PageHub::new(doc)));
        hub.sessions.fetch_add(1, Ordering::AcqRel);
        Ok(Arc::clone(hub))
    }

    /// Detach a session, dropping the hub once the last one leaves.
    pub async fn detach_hub(&self, page_id: Uuid, hub: &Arc<PageHub>) {
        let mut hubs = self.hubs.lock().await;
        if hub.sessions.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Count reached zero while we hold the registry lock, so no session
            // can be attaching to this hub right now. The ptr_eq check is
            // defensive: only ever remove the hub we were actually holding.
            if hubs.get(&page_id).is_some_and(|h| Arc::ptr_eq(h, hub)) {
                hubs.remove(&page_id);
            }
        }
    }

    /// Number of live hubs. Test-only visibility into cleanup.
    pub async fn hub_count(&self) -> usize {
        self.hubs.lock().await.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("failed to load doc: {0}")]
    Load(#[from] sqlx::Error),
    #[error("corrupt doc log: {0}")]
    Corrupt(String),
}
