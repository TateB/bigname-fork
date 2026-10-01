-- Existing schema-v2 databases gain the index the name-ordered readers walk:
-- the /v1/search candidates (storage:families.name.search_candidates), a resolver's
-- bound names (storage:families.name.bound_candidates) and reverse lookup candidates
-- (storage:families.records.reverse_candidates) all read readable surfaces by raw
-- name, namespace and namehash, which no index served, so each batch sorted every
-- match. Names longer than 2000 bytes stay out: an index entry larger than about
-- 2.7 KB fails the insert, and those readers carry the same bound.
-- Index only; no column or row changes. An empty schema-migration database has no
-- phase baseline yet, so this schema-migration is a no-op there and phase-runner
-- init-schema installs the same index.
DO $migration$
BEGIN
IF to_regclass('bigname_phase.name_surfaces') IS NULL THEN
    RETURN;
END IF;

EXECUTE $ddl$
CREATE INDEX IF NOT EXISTS name_surfaces_name_order_idx
    ON bigname_phase.name_surfaces (raw_name, namespace, namehash, logical_name_id)
    WHERE visibility_state = 'active'
      AND canonicality_state IN ('canonical', 'safe', 'finalized')
      AND octet_length(raw_name) <= 2000
$ddl$;
END
$migration$;
