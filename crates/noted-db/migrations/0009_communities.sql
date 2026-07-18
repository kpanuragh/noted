-- M2b: communities (clusters of entities), their summaries, and the churn
-- counter that decides when to re-cluster.
--
-- SCOPING, STATED RATHER THAN LEFT TO BE RE-DERIVED. M2a produced four separate
-- data bugs from one misunderstanding, and the rule it finally wrote down is:
-- a globally-shared, content-addressed key is safe when it gates a PER-CONTENT
-- write and unsafe when it gates a PER-TENANT write. There is NO content-addressed
-- key anywhere in this migration — `member_set_hash` looks like one and is not:
-- it hashes entity IDS, which are per-workspace rows (`entities` is unique on
-- (workspace_id, name), and the same name in two workspaces is two different
-- nodes), so the same real-world community in two tenants hashes differently and
-- can never be confused for one shared artifact. The M2a trap therefore cannot
-- recur here. Every table below is still explicitly per-tenant, and each one
-- says how it gets there.

-- Communities are per-workspace because their MEMBERS are: a community is a set
-- of `entities` rows, and those are already partitioned by tenant. The
-- `workspace_id` column is not redundant with that — it is what lets a swap
-- delete "this workspace's whole partition" in one statement without joining
-- through members, and what keeps a workspace with an empty partition
-- describable at all.
--
-- UNIQUE (workspace_id, level, member_set_hash) is a real invariant, not a
-- convenience index: a partition's communities are DISJOINT, so within one
-- workspace and level no two can have the same member set, and therefore no two
-- can share a hash. Making it a constraint is what lets `swap_partition` match
-- an incoming community to the existing row describing the same members, and so
-- preserve that row's id — which is the whole reason `member_set_hash` exists.
-- Without the preservation, `community_summaries` (keyed by `community_id`,
-- ON DELETE CASCADE) would be wiped by every cold run and every summary would
-- be regenerated even when nothing moved.
--
-- `level` is Louvain's hierarchy level. It is part of the key because the same
-- member set can legitimately appear at two levels (a level-0 community that no
-- higher pass merged), and those are different rows describing different things.
CREATE TABLE communities (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    level           int  NOT NULL DEFAULT 0,
    member_set_hash text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, level, member_set_hash)
);
CREATE INDEX communities_workspace_idx ON communities (workspace_id, level);

-- Membership. Deliberately NO `workspace_id` column: `community_id` already
-- determines the workspace, and `entity_id` independently does too. Denormalising
-- it here would create a third copy that can disagree with the other two, and
-- there is no query that needs it — this is the opposite situation to `edges`,
-- which needed `workspace_id` added in 0007 precisely because its
-- `source_chunk_hash` is a GLOBAL content-addressed key that carries no tenant
-- at all. Nothing here is content-addressed, so nothing here needs the column.
--
-- ON DELETE CASCADE from `entities`: reaping an entity (the M2b-1 prerequisite —
-- nothing in the system removes graph nodes today) must not leave a community
-- pointing at a node that no longer exists. NOTE the consequence, since it is
-- the one way a community's stored hash can go stale: such a delete changes the
-- member set WITHOUT updating `member_set_hash`, so the row then describes a
-- membership it no longer has until the next cold run replaces it. That is
-- acceptable — it makes the community look UNCHANGED and merely defers a summary
-- regeneration, which is the same "stale but usable" direction §2.2 already
-- chose deliberately — but it is the reason the hash must never be treated as
-- authoritative over a live count of the members.
CREATE TABLE community_members (
    community_id uuid NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
    entity_id    uuid NOT NULL REFERENCES entities(id)    ON DELETE CASCADE,
    PRIMARY KEY (community_id, entity_id)
);
-- The reverse lookup: "which community does this entity belong to". The hot
-- path asks exactly that, per affected entity, on every edge change.
CREATE INDEX community_members_entity_idx ON community_members (entity_id);

-- One summary per community, keyed by `community_id` alone — NOT by
-- (community_id, model_id) the way `embeddings` is keyed (content_hash,
-- model_id). The asymmetry is deliberate (design §3): embeddings must let two
-- models coexist because a query embedded under model X can only search vectors
-- from model X, whereas a summary is prose for humans and for map-reduce, there
-- is no use case for two summariser models at once, and changing the model is a
-- full regeneration anyway. `model_id` is therefore an attribute recording what
-- WROTE this summary, not part of its identity.
--
-- Per-tenant via `community_id` for the same reason `community_members` is.
--
-- `member_set_hash` here is what the summary was generated FOR; compare it to
-- the owning community's current hash to decide validity. It is duplicated
-- rather than joined-to on purpose: after a swap the community row may describe
-- a NEW membership while this row still records the old one, and that difference
-- IS the staleness signal.
--
-- `state` is 'valid' | 'stale_usable'. Left as free text with no CHECK
-- constraint: M2b-3 may well need a third state, and a CHECK would make that a
-- migration; the column is written by exactly one module, which is where the
-- domain is enforced. Recorded as a decision so the absence is not read as an
-- oversight.
CREATE TABLE community_summaries (
    community_id    uuid PRIMARY KEY REFERENCES communities(id) ON DELETE CASCADE,
    model_id        text NOT NULL,
    summary         text NOT NULL,
    state           text NOT NULL,
    member_set_hash text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- The churn counter driving the cold path: edges changed since the last full
-- clustering run. Keyed by `workspace_id` ALONE, which is the whole point —
-- clustering is a per-workspace operation over a per-workspace graph, so one
-- tenant's edit storm must never drag another tenant into a full re-cluster. A
-- global counter would do exactly that, and on a shared instance the busiest
-- workspace would keep every other workspace permanently re-clustering.
--
-- A workspace with no row here has changed no edges and has never had a full
-- run; the repository reads that as (0, NULL) rather than requiring the row to
-- be created up front, so a brand-new workspace needs no initialisation.
CREATE TABLE graph_churn (
    workspace_id     uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    edges_changed    bigint NOT NULL DEFAULT 0,
    last_full_run_at timestamptz
);
