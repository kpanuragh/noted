-- `chunk_extractions` was keyed (content_hash, model_id) — GLOBAL, with no
-- workspace column. But `content_hash` is a content-addressed key shared
-- across tenants (M1b: two workspaces whose page text is byte-identical
-- reference the SAME chunk row), while the graph an extraction produces
-- (entities/edges) is strictly per-workspace.
--
-- The consequence was a silent, permanent data gap. Workspace A extracts
-- chunk `h` and marks it. Later — after A has fully drained — workspace B
-- creates a page with byte-identical text, so `page_chunks` points B at that
-- same `h`. `pending_extraction` LEFT JOINed the marker on (content_hash,
-- model_id) only, saw `h` already marked, and never queued it. B got NO graph
-- for that chunk, ever, and `extraction_progress(ws_b)` reported 1/1 success
-- while doing so. That violates M2a's stated invariant: a chunk shared across
-- workspaces extracts into EACH workspace's graph.
--
-- The marker is therefore per-workspace: it records "THIS workspace's graph
-- has been written for this chunk under this model", which is the thing the
-- queue actually needs to know.
--
-- Rebuild rather than ALTER: the primary key gains a leading column AND
-- existing rows must FAN OUT (one global row -> one row per referencing
-- workspace), which an in-place ALTER cannot express.
CREATE TABLE chunk_extractions_new (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    content_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id     text NOT NULL,
    extracted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, content_hash, model_id)
);

-- Backfill by resolving each global marker to every workspace that actually
-- references the chunk, via page_chunks -> pages. Two deliberate decisions:
--
--  1. A marker whose chunk is referenced by N workspaces becomes N rows. The
--     old row asserted "extracted" without saying for whom; the only honest
--     reading is that the extraction that wrote it ran through the worker,
--     which fans out over `workspaces_for_chunk` and writes EVERY referencing
--     workspace's edges before marking. So all N were in fact written.
--
--  2. A marker whose chunk is referenced by NO workspace is DROPPED. There is
--     no workspace to attribute it to and the new PK cannot represent it.
--     Losing it is harmless and self-correcting: such a chunk is orphaned, so
--     `pending_extraction` (which joins through live `page_chunks`) does not
--     return it either way. If a page ever references that text again, the
--     chunk re-enters the queue and is re-extracted — which is the correct
--     outcome, because that new page's workspace genuinely has no graph yet.
--
-- Archived pages are NOT filtered here. The queue treats archived pages as
-- dead, so an archived-only marker can never be re-checked anyway; keeping it
-- simply means un-archiving a page does not force a needless re-extraction of
-- a graph that was already written.
INSERT INTO chunk_extractions_new (workspace_id, content_hash, model_id, extracted_at)
SELECT DISTINCT p.workspace_id, ce.content_hash, ce.model_id, ce.extracted_at
FROM chunk_extractions ce
JOIN page_chunks pc ON pc.content_hash = ce.content_hash
JOIN pages p ON p.id = pc.page_id;

DROP TABLE chunk_extractions;
ALTER TABLE chunk_extractions_new RENAME TO chunk_extractions;
