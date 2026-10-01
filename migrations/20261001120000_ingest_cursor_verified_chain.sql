-- Existing schema-v2 databases gain the RPC chain identity each ingest cursor's endpoint
-- reported: the EIP-155 chain id, and the block 0 hash when the runner checked it. Both stay
-- NULL until the next verified start records them. Columns only; no row changes. An empty
-- schema-migration database has no phase baseline yet, so this schema-migration is a no-op there
-- and phase-runner init-schema installs the same columns.
DO $migration$
BEGIN
IF to_regclass('bigname_phase.ingest_cursors') IS NULL THEN
    RETURN;
END IF;

ALTER TABLE bigname_phase.ingest_cursors
    ADD COLUMN IF NOT EXISTS verified_chain_id bigint,
    ADD COLUMN IF NOT EXISTS verified_genesis_hash text;
END
$migration$;
