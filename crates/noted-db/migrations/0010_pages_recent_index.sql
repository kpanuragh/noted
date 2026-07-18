-- The index behind `pages::recent` (the dashboard's "recently edited" list).
--
-- Shape follows the query exactly: equality on workspace_id, then the full
-- ORDER BY (updated_at DESC, id DESC) so the index supplies the ordering and
-- the LIMIT stops the scan early — no sort node, no reading the tenant's whole
-- page set. The `id DESC` tiebreak is in the index for the same reason it is in
-- the query: `updated_at` is not unique, and this repository's convention
-- (chunks::pending, graph::pending_extraction, search::related_pages) is that
-- every LIMIT gets a deterministic order.
--
-- PARTIAL on `archived_at IS NULL`, matching the one definition of "live" that
-- chunks::pending, chunks::progress, graph::pending_extraction,
-- graph::extraction_progress and pages::all_page_ids already share. Archived
-- pages are deleted as far as the user is concerned and can never appear in
-- this list, so keeping them out of the index keeps it smaller and lets the
-- planner drop the predicate entirely.
--
-- COST, stated rather than glossed: `docs::append` now bumps pages.updated_at
-- on every persisted document update, and updated_at is indexed here — so those
-- updates cannot be HOT and each one writes a new entry into every index on
-- `pages`, including the GIN trigram index on title. See the write-amplification
-- note on `pages::recent`.
CREATE INDEX pages_workspace_updated_idx
    ON pages (workspace_id, updated_at DESC, id DESC)
    WHERE archived_at IS NULL;
