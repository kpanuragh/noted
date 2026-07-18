-- edges' PK is (source_entity, target_entity, relation, source_chunk_hash, model_id),
-- which already leads with source_entity, so Postgres uses the PK index for any
-- source_entity lookup. The separate edges_source_entity_idx was pure write cost.
DROP INDEX IF EXISTS edges_source_entity_idx;
