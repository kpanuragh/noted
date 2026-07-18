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

-- Backfill from the EVIDENCE, not from the marker.
--
-- The old global row asserts "extracted" without saying for whom, so it has to
-- be attributed to workspaces somehow. The tempting attribution — fan it out
-- over every workspace that references the chunk via page_chunks -> pages — is
-- WRONG, and wrong in exactly the direction this migration exists to fix. It
-- is true that the worker fans out over `workspaces_for_chunk` and writes
-- every referencing workspace's edges before marking, but only for the
-- workspaces referencing the chunk AT THE MOMENT OF EXTRACTION. A workspace
-- that adopted the chunk LATER — the precise population described in the
-- header above — got no edges written and was never queued. Fanning out would
-- stamp it "extracted" and strand it forever.
--
-- So: carry over a marker for `(workspace_id, content_hash, model_id)` only
-- where `edges` actually holds that workspace's edges for that chunk and
-- model. `edges` gained `workspace_id` in 0007, so this evidence is available.
--
-- THE GOVERNING ASYMMETRY, since the two error directions are not comparable:
--
--   * OVER-marking (claiming a workspace has a graph it does not) is
--     PERMANENT, SILENT and UNBOUNDED. The queue is a set difference against
--     this table, so an over-marked row is never revisited; `pending_extraction`
--     skips it and `extraction_progress` reports it complete. Nothing in the
--     system ever notices.
--
--   * UNDER-marking (re-queuing a workspace that already has a graph) costs
--     only a redundant model call, and it is SELF-CORRECTING:
--     `replace_chunk_edges` is idempotent — it deletes that workspace's edges
--     for the chunk+model and rewrites them in one transaction, then re-writes
--     the marker with ON CONFLICT DO NOTHING.
--
-- When those are the error directions, prefer under-marking.
--
-- WHAT THIS COSTS, stated plainly rather than glossed over: an extraction that
-- legitimately produced NO entities (a chunk with nothing to extract) leaves no
-- edges, so it is indistinguishable from "never extracted" and will be
-- re-extracted exactly once after this migration. That is a bounded, one-off
-- redundant model call per empty chunk, and it is the correct side of the
-- trade above.
--
-- A marker whose chunk is referenced by NO workspace is likewise dropped, both
-- because it has no edges to evidence it and because the new PK cannot
-- represent it. Harmless: `pending_extraction` joins through live
-- `page_chunks` and never returns an orphaned chunk either way.
--
-- Archived pages are NOT filtered here. The queue treats archived pages as
-- dead, so an archived-only marker can never be re-checked anyway; keeping it
-- simply means un-archiving a page does not force a needless re-extraction of
-- a graph that was already written.
INSERT INTO chunk_extractions_new (workspace_id, content_hash, model_id, extracted_at)
SELECT DISTINCT p.workspace_id, ce.content_hash, ce.model_id, ce.extracted_at
FROM chunk_extractions ce
JOIN page_chunks pc ON pc.content_hash = ce.content_hash
JOIN pages p ON p.id = pc.page_id
WHERE EXISTS (
    SELECT 1
    FROM edges e
    WHERE e.workspace_id = p.workspace_id
      AND e.source_chunk_hash = ce.content_hash
      AND e.model_id = ce.model_id
);

DROP TABLE chunk_extractions;
ALTER TABLE chunk_extractions_new RENAME TO chunk_extractions;

-- `ALTER TABLE ... RENAME` renames the TABLE only; dependent constraints and
-- their backing indexes keep the names they were created with. Left alone,
-- this table's primary key would be `chunk_extractions_new_pkey` forever, so a
-- future migration naming `chunk_extractions_pkey` — the name every other
-- table in this schema has — would fail against a scaffolding artefact of how
-- 0008 happened to be written. Renaming a constraint also renames its index.
ALTER TABLE chunk_extractions
    RENAME CONSTRAINT chunk_extractions_new_pkey TO chunk_extractions_pkey;
ALTER TABLE chunk_extractions
    RENAME CONSTRAINT chunk_extractions_new_content_hash_fkey TO chunk_extractions_content_hash_fkey;
ALTER TABLE chunk_extractions
    RENAME CONSTRAINT chunk_extractions_new_workspace_id_fkey TO chunk_extractions_workspace_id_fkey;
