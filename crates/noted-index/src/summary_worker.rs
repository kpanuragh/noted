//! The summary worker: keep every community's summary in step with its
//! membership, while making as few model calls as possible.
//!
//! # The queue is a QUERY, not a table
//!
//! Same shape M1b's embedding queue and M2a's extraction queue are built on,
//! and for the same three reasons: there is no status column to get out of
//! step with reality, no claim/lease state to strand on a crash, and killing
//! the process mid-pass costs nothing because the next poll re-evaluates the
//! set difference from scratch. A community needs a summary exactly when
//! `communities` has a row that `community_summaries` does not describe —
//! which is a `LEFT JOIN`, not a state machine. See [`pending_summaries`].
//!
//! # Lazy invalidation, and where staleness actually comes from
//!
//! Design §2.2 asks for three behaviours, and they map onto the two paths that
//! can change a community:
//!
//!   * **Unchanged `member_set_hash` ⇒ the summary is still valid, full stop.**
//!     Not "probably valid" — the hash is over the sorted member ids, so an
//!     equal hash IS an equal member set. Those communities never enter the
//!     queue and cost ZERO model calls, however many cold runs have swept over
//!     them. `swap_partition` is what makes this reachable at all: it preserves
//!     the existing row (and therefore its summary, which cascades on delete)
//!     whenever `(level, member_set_hash)` already exists.
//!   * **A community whose membership changed IN PLACE** — that is the HOT
//!     path, `community::reassign_entity`, which updates `member_set_hash` on
//!     the surviving row. The summary row survives too, still carrying the hash
//!     it was generated for, and that difference is the staleness signal.
//!   * **A community with no summary at all** — brand new, or one whose
//!     predecessor row was deleted by a cold-run swap (which cascades the
//!     summary away, because a summary describes a membership that no longer
//!     exists). There is nothing to serve, so there is nothing to be lazy with.
//!
//! # No drain loop, no failure cap
//!
//! Unlike `extract_worker`/`worker` there is no batch paging here: one pass
//! covers a whole workspace's communities, of which a workspace has tens, not
//! millions. With no loop there is nothing that could spin, so this deliberately
//! ships NO `MAX_CONSECUTIVE_FAILURES` analogue. A cap on a loop that does not
//! exist would be a mechanism no mutation could kill — precisely the defect
//! Task 2 and Task 3 each found once. A community whose summariser keeps
//! failing simply stays in the queue and is retried by the next pass, which is
//! the same fate `extract_worker` gives a poison chunk.
use std::sync::Arc;

use noted_db::community::member_set_hash;
use uuid::Uuid;

use crate::summary::{CommunityFacts, CommunityMember, SummaryProvider, verify_summary};

/// The small/large boundary, in MEMBERS: a community with at least this many
/// members whose membership changed keeps serving its old summary
/// (`stale_usable`) instead of being regenerated on the spot.
///
/// # Where 20 comes from — it is derived, not picked
///
/// It is `ceil(1 / CHURN_THRESHOLD_FRACTION)`: the smallest community in which
/// ONE membership change is within the same 5% tolerance the cold path already
/// applies to the partition as a whole. The system has exactly one number
/// expressing "how much drift is acceptable before the approximation must be
/// corrected", and this reuses it rather than inventing a second, unrelated
/// one that would then silently disagree with the first. Loosen the churn
/// threshold and this boundary loosens with it; `the_stale_usable_boundary_is_derived_from_the_churn_threshold`
/// in `tests/summary.rs` fails if the two ever drift apart.
///
/// The unit of hot-path change is one entity moving (`community::reassign_entity`
/// moves exactly one), so "one change" is the right thing to measure against.
///
/// # Why community SIZE and not change SIZE — an honest limitation
///
/// The direct measure would be `|old members Δ new members|`, and it is NOT
/// COMPUTABLE from the current schema. `community_summaries` records only the
/// old membership's HASH, which is one-way by construction; the old member
/// list is gone. Recovering it would need a column (`member_ids uuid[]`, or at
/// minimum `member_count int`) that migration `0009_communities.sql` does not
/// have.
///
/// Community size is the available proxy, and it is a defensible one rather
/// than a shrug: one entity joining a 3-member community changes a third of
/// what the prose is about and the old summary is likely to be actively wrong,
/// while one entity joining a 30-member community changes 3% of it and the old
/// prose is still a fair description of the cluster. It errs in the safe
/// direction too — a LARGE change to a large community is classified lazy when
/// it should be eager, and the cost of that is a summary that reads slightly
/// out of date until the next pass, which is exactly the trade §2.2 chose
/// deliberately ("a slightly stale summary is far better than a missing one").
/// The opposite error — treating a missing summary as usable — cannot occur,
/// because that class is decided by the presence of a row, not by size.
pub const STALE_USABLE_MIN_MEMBERS: i64 = 20;

/// `community_summaries.state` values. Free text in the schema on purpose (see
/// `0009_communities.sql`); this module is the one place that writes them, so
/// this is where the domain lives.
pub const STATE_VALID: &str = "valid";
pub const STATE_STALE_USABLE: &str = "stale_usable";

#[derive(Debug, thiserror::Error)]
pub enum SummaryWorkerError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// How a community that needs attention is going to get it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Regenerate NOW: there is no usable summary, or the community is small
    /// enough that a membership change plausibly changed what it is about.
    Eager,
    /// Keep serving the existing summary, marked `stale_usable`, and regenerate
    /// on demand ([`SummaryWorker::refresh`]) or in a later pass.
    Lazy,
}

/// One row of the set-difference queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSummary {
    pub community_id: Uuid,
    pub level: i32,
    /// The community's CURRENT member count.
    pub member_count: i64,
    /// Whether a summary row exists at all (regardless of how stale).
    pub has_summary: bool,
    /// Whether the existing summary was written by a DIFFERENT summariser.
    /// False when there is no summary at all — a missing summary is not a model
    /// change, and conflating them would make the two classification inputs
    /// impossible to tell apart in a test.
    pub model_changed: bool,
    pub urgency: Urgency,
}

/// What one [`SummaryWorker::run_once`] pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SummaryPass {
    /// Communities the summariser was called for and whose summary was stored.
    pub regenerated: usize,
    /// Communities left serving an existing summary, marked `stale_usable`.
    /// **No model call was made for these.**
    pub marked_stale: usize,
    /// Communities whose summariser call failed or returned nothing usable.
    /// They stay in the queue.
    pub failed: usize,
}

/// Which workspaces have communities awaiting a summary under `model_id`.
///
/// The scheduler is instance-wide but `SummaryWorker` is workspace-scoped —
/// deliberately, since communities are a per-tenant artifact — so the loop
/// needs to know which tenants have work before it can do any. Same
/// set-difference shape as [`pending_summaries`], collapsed to the workspace.
///
/// `limit` bounds one pass: a workspace with thousands of pending communities
/// must not starve every other workspace on the instance. The remainder is
/// simply still pending on the next pass, because the queue is a QUERY rather
/// than a cursor — there is no position to lose.
///
/// ORDERED, and that is load-bearing rather than tidiness. `LIMIT` without
/// `ORDER BY` lets Postgres return any rows it likes, and in practice it
/// returns the SAME arbitrary rows every time — so the same few workspaces
/// would be served on every pass while the rest starved indefinitely. Not
/// hypothetical: pointing this at a real database found 1867 workspaces
/// pending under a new model id, of which a bounded, unordered query would
/// have drained four, forever.
///
/// Oldest pending community first, so the queue drains FIFO and a workspace
/// leaves it as soon as its summaries are written. A workspace whose
/// summariser calls keep failing does hold its slot — but it holds one of
/// `limit`, not all of them.
pub async fn workspaces_with_pending_summaries(
    pool: &sqlx::PgPool,
    model_id: &str,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT c.workspace_id
         FROM communities c
         LEFT JOIN community_summaries s ON s.community_id = c.id
         WHERE s.community_id IS NULL
            OR s.member_set_hash IS DISTINCT FROM c.member_set_hash
            OR s.model_id IS DISTINCT FROM $1
         GROUP BY c.workspace_id
         ORDER BY min(c.created_at)
         LIMIT $2",
    )
    .bind(model_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// The set-difference queue, in one query.
///
/// A community is pending when `community_summaries` holds no row for it, or
/// holds one whose `member_set_hash` differs from the community's current hash,
/// or holds one written by a DIFFERENT summariser model. Everything else — the
/// overwhelmingly common case after a cold run that moved nothing — is absent
/// from the result, which is what makes "unchanged membership ⇒ zero model
/// calls" true by construction rather than by an early-return somebody could
/// delete.
///
/// The three predicates are separate on purpose and each is load-bearing:
///
///   * `s.community_id IS NULL` — brand-new community (or one whose row a cold
///     swap replaced, cascading the old summary away).
///   * `s.member_set_hash IS DISTINCT FROM c.member_set_hash` — the hot path
///     moved a member in or out of a surviving row.
///   * `s.model_id IS DISTINCT FROM $2` — the summariser changed. Design §3
///     keys `community_summaries` by `community_id` ALONE (unlike `embeddings`,
///     keyed `(content_hash, model_id)` so two models coexist) precisely
///     because a model change is a full regeneration; this predicate is what
///     makes that true instead of aspirational. Without it, switching models
///     would leave every community holding the old model's prose forever, since
///     nothing else about it would look stale.
///
/// `IS DISTINCT FROM` rather than `<>` so a NULL on either side compares as
/// different rather than swallowing the row into SQL's three-valued logic —
/// both columns are `NOT NULL` today, so this is defence in depth and is
/// documented as such rather than left to imply a live path.
///
/// `ORDER BY (level, member_set_hash)` — deterministic (M1c's lesson), and
/// specifically NOT `ORDER BY c.id`, which is `gen_random_uuid()` and therefore
/// stable within one database but not across a rebuild.
///
/// MEASURED, AND STATED RATHER THAN IMPLIED: deleting this ORDER BY kills no
/// test, and cannot. A pass consumes the WHOLE queue and no failure aborts it,
/// so the order communities are visited in is unobservable today. It stays
/// because the moment anyone adds a `LIMIT` — the obvious next step if a
/// workspace ever has enough communities to want batching — an unordered queue
/// silently starts returning overlapping and gap-ridden batches, and that bug
/// is far cheaper to prevent than to find. Documented as a guard for a future
/// change, NOT as a live mechanism, which is the correction Task 3 had to make
/// to `clusterable_graph`'s defence-in-depth predicate.
pub async fn pending_summaries(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    model_id: &str,
) -> Result<Vec<PendingSummary>, sqlx::Error> {
    let rows: Vec<(Uuid, i32, i64, bool, bool)> = sqlx::query_as(
        "SELECT c.id,
                c.level,
                (SELECT count(*) FROM community_members cm WHERE cm.community_id = c.id),
                (s.community_id IS NOT NULL),
                COALESCE(s.model_id IS DISTINCT FROM $2, false)
         FROM communities c
         LEFT JOIN community_summaries s ON s.community_id = c.id
         WHERE c.workspace_id = $1
           AND (s.community_id IS NULL
                OR s.member_set_hash IS DISTINCT FROM c.member_set_hash
                OR s.model_id IS DISTINCT FROM $2)
         ORDER BY c.level, c.member_set_hash",
    )
    .bind(workspace_id)
    .bind(model_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(community_id, level, member_count, has_summary, model_changed)| PendingSummary {
                community_id,
                level,
                member_count,
                has_summary,
                model_changed,
                urgency: classify(has_summary, model_changed, member_count),
            },
        )
        .collect())
}

/// The small-vs-large decision, isolated so it is testable without a database.
///
/// Two things force [`Urgency::Eager`] regardless of size:
///
///   * **No summary at all.** Laziness means "keep serving the old one", and
///     there is no old one. A brand-new community has nothing to degrade
///     gracefully to.
///   * **A different summariser wrote it.** "Slightly stale" is a claim about
///     MEMBERSHIP drift — the prose still describes most of the same cluster.
///     It says nothing about prose written by another model, which may differ
///     in language, length, format or quality, and which the operator changed
///     the model precisely in order to replace. Design §3 keys
///     `community_summaries` by `community_id` alone on the explicit reasoning
///     that "a model change is a full regeneration anyway"; classifying it lazy
///     would make that sentence false for every community above the boundary.
///
/// Otherwise the community is large enough for one membership change to be
/// within tolerance ([`STALE_USABLE_MIN_MEMBERS`]) or it is not.
pub fn classify(has_summary: bool, model_changed: bool, member_count: i64) -> Urgency {
    if has_summary && !model_changed && member_count >= STALE_USABLE_MIN_MEMBERS {
        Urgency::Lazy
    } else {
        Urgency::Eager
    }
}

/// Workspace-scoped, always — like `CommunityWorker`, and for the same reason:
/// communities are a per-tenant artifact and there is no such thing as a
/// faster whole-instance summary run, only a data-integrity bug.
pub struct SummaryWorker {
    pool: sqlx::PgPool,
    provider: Arc<dyn SummaryProvider>,
    workspace_id: Uuid,
}

impl SummaryWorker {
    pub fn new(pool: sqlx::PgPool, provider: Arc<dyn SummaryProvider>, workspace_id: Uuid) -> Self {
        Self {
            pool,
            provider,
            workspace_id,
        }
    }

    /// The community's members, **ordered by `entities.name` ascending**.
    ///
    /// The ordering is load-bearing, not tidiness. A summariser's output is a
    /// function of its input, so an unordered member list would hand the same
    /// community a different prompt on different passes and produce different
    /// prose for a membership that never changed — non-determinism entering
    /// through the one seam the whole `member_set_hash` economy assumes is
    /// closed. `name` and not `id` for the reason the clusterer orders by name
    /// (`community::clusterable_graph`): `entities.id` is `gen_random_uuid()`,
    /// stable within one database and not across a rebuild.
    ///
    /// The join's `c.workspace_id = $2` is DEFENCE IN DEPTH, not the
    /// load-bearing scope, and is documented as such rather than left to imply
    /// a hot path — the same correction M2a made to `replace_chunk_edges`'s
    /// `ON CONFLICT` clause and Task 3 made to `clusterable_graph`'s node
    /// query. Measured: deleting it kills no test, because every community id
    /// that reaches here came from `pending_summaries`, which is already scoped
    /// (and `refresh` routes through that same query rather than trusting its
    /// argument). It stays because it is index-backed and free, and because it
    /// is what would contain a future caller that passed an unvetted id.
    async fn facts(&self, community_id: Uuid, level: i32) -> Result<CommunityFacts, sqlx::Error> {
        let rows: Vec<(Uuid, String, String, Option<String>)> = sqlx::query_as(
            "SELECT e.id, e.name, e.entity_type, e.description
             FROM community_members cm
             JOIN communities c ON c.id = cm.community_id AND c.workspace_id = $2
             JOIN entities e    ON e.id = cm.entity_id
             WHERE cm.community_id = $1
             ORDER BY e.name ASC",
        )
        .bind(community_id)
        .bind(self.workspace_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(CommunityFacts {
            community_id,
            level,
            members: rows
                .into_iter()
                .map(|(id, name, entity_type, description)| CommunityMember {
                    id,
                    name,
                    entity_type,
                    description,
                })
                .collect(),
        })
    }

    /// Summarise one community and store the result as `valid`.
    ///
    /// # The stored hash comes from the members that were actually summarised
    ///
    /// It is recomputed here from the ids [`Self::facts`] returned, NOT copied
    /// from `communities.member_set_hash`. Those can differ: the hot path can
    /// commit a move between the queue poll and this read, and copying the row's
    /// hash would then stamp the summary as describing a membership it does not
    /// describe. That error is PERMANENT and SILENT — the set-difference queue
    /// would see a matching hash and never revisit the community. Recomputing
    /// can only err the other way: the stored hash may lag the row's, the
    /// community stays in the queue, and the next pass costs one redundant
    /// model call. That is the error-direction rule M2a's 0008 backfill wrote
    /// down — prefer under-marking, and attribute from the evidence you
    /// actually have rather than from a marker you did not verify.
    ///
    /// `ON CONFLICT` because a stale row is being replaced far more often than
    /// a new one is inserted, and `created_at` is refreshed with it so the
    /// column means "when this summary was written" rather than "when this
    /// community was first ever summarised".
    ///
    /// The `INSERT ... SELECT FROM communities WHERE c.workspace_id = $6` shape
    /// (rather than a bare `VALUES`) is DEFENCE IN DEPTH, measured and stated:
    /// deleting the workspace predicate kills no test, for the same reason it
    /// does not in `facts` — every id reaching here came from the scoped queue.
    /// It stays because it makes a cross-tenant write structurally impossible
    /// rather than merely unreached.
    async fn regenerate(&self, community_id: Uuid, level: i32) -> Result<bool, SummaryWorkerError> {
        let facts = self.facts(community_id, level).await?;
        let hash = member_set_hash(&facts.members.iter().map(|m| m.id).collect::<Vec<_>>());
        let model_id = self.provider.model_id().to_string();

        let summary = match self.provider.summarise(&facts).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    community = %community_id,
                    error = %err,
                    "community could not be summarised; leaving it in the queue"
                );
                return Ok(false);
            }
        };

        // A model that "succeeds" with nothing in it is a failure that has not
        // noticed yet. Same fate as an outright error: skipped, left queued.
        if let Err(err) = verify_summary(&summary, &model_id) {
            tracing::warn!(
                community = %community_id,
                error = %err,
                "summariser returned nothing usable; leaving it in the queue"
            );
            return Ok(false);
        }

        sqlx::query(
            "INSERT INTO community_summaries
                 (community_id, model_id, summary, state, member_set_hash)
             SELECT c.id, $2, $3, $4, $5
             FROM communities c
             WHERE c.id = $1 AND c.workspace_id = $6
             ON CONFLICT (community_id) DO UPDATE
               SET model_id        = EXCLUDED.model_id,
                   summary         = EXCLUDED.summary,
                   state           = EXCLUDED.state,
                   member_set_hash = EXCLUDED.member_set_hash,
                   created_at      = now()",
        )
        .bind(community_id)
        .bind(&model_id)
        .bind(&summary)
        .bind(STATE_VALID)
        .bind(&hash)
        .bind(self.workspace_id)
        .execute(&self.pool)
        .await?;

        Ok(true)
    }

    /// Flag an existing summary as approximate WITHOUT calling the summariser.
    ///
    /// This is the whole lazy path: the prose stays queryable and a reader can
    /// see that it is behind. Scoped through `communities` so it cannot touch
    /// another tenant's row.
    ///
    /// An `AND s.state <> $2` guard was written here and then REMOVED. It only
    /// avoided rewriting a row with the value it already held, which is
    /// unobservable — no mutation could kill it, and this project has now found
    /// three such mechanisms (Task 2's, Task 3's `max(1.0)` floor, and this
    /// one). An idempotent update is cheaper to reason about than a condition
    /// nothing can distinguish.
    async fn mark_stale(&self, community_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE community_summaries s
             SET state = $2
             FROM communities c
             WHERE c.id = s.community_id
               AND s.community_id = $1
               AND c.workspace_id = $3",
        )
        .bind(community_id)
        .bind(STATE_STALE_USABLE)
        .bind(self.workspace_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// ONE PASS over the workspace. Eager communities are regenerated; lazy
    /// ones are marked `stale_usable` and left alone.
    ///
    /// A single community's failure is isolated (logged, counted, skipped, left
    /// in the queue) and never stops the pass — one community whose prose the
    /// model cannot produce must not deny every other community its summary.
    /// A `sqlx::Error`, by contrast, propagates: that is a dead database, not a
    /// poison community, and retrying past it would silently lose work.
    /// Same split `extract_worker::process_batch` makes.
    pub async fn run_once(&self) -> Result<SummaryPass, SummaryWorkerError> {
        let model_id = self.provider.model_id().to_string();
        let pending = pending_summaries(&self.pool, self.workspace_id, &model_id).await?;

        let mut pass = SummaryPass::default();
        for p in pending {
            match p.urgency {
                Urgency::Lazy => {
                    self.mark_stale(p.community_id).await?;
                    pass.marked_stale += 1;
                }
                Urgency::Eager => {
                    if self.regenerate(p.community_id, p.level).await? {
                        pass.regenerated += 1;
                    } else {
                        pass.failed += 1;
                    }
                }
            }
        }
        Ok(pass)
    }

    /// THE LAZY PATH'S TRIGGER. Regenerate ONE community, on demand — what a
    /// global search does when it touches a summary marked `stale_usable`.
    ///
    /// Returns whether the summariser was called and its output stored. A
    /// community whose summary is already current for this model returns
    /// `Ok(false)` having made **zero** model calls: the same set-difference
    /// query decides, so the lazy path can be called speculatively on every
    /// read without a cost model of its own.
    pub async fn refresh(&self, community_id: Uuid) -> Result<bool, SummaryWorkerError> {
        let model_id = self.provider.model_id().to_string();
        let pending = pending_summaries(&self.pool, self.workspace_id, &model_id).await?;
        match pending.into_iter().find(|p| p.community_id == community_id) {
            Some(p) => self.regenerate(p.community_id, p.level).await,
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_worker::CHURN_THRESHOLD_FRACTION;

    /// The boundary is DERIVED from the churn threshold, not chosen next to it.
    /// If someone retunes `CHURN_THRESHOLD_FRACTION` and this constant stays
    /// put, the system holds two different opinions about how much drift is
    /// acceptable and neither comment says so. This fails the moment they part.
    #[test]
    fn the_stale_usable_boundary_is_derived_from_the_churn_threshold() {
        assert_eq!(
            STALE_USABLE_MIN_MEMBERS,
            (1.0 / CHURN_THRESHOLD_FRACTION).ceil() as i64,
            "STALE_USABLE_MIN_MEMBERS must stay ceil(1 / CHURN_THRESHOLD_FRACTION): the smallest \
             community in which one membership change is within the same tolerance the cold path \
             applies to the partition"
        );
    }
}
