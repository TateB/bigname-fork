-- The Basenames Base twins of the ENSv1 lookahead loader indexes installed by
-- 20260917150000_normalized_events_v1_lookahead_indexes.sql: the same expressions over the
-- basenames_base_* families, so Interpret's lookahead loader can serve Base. Prebuild them
-- concurrently on large initialized databases using ops/v1-lookahead-indexes/install.sql
-- before applying schema-migrations.
--
-- Index only; no row changes. CREATE INDEX IF NOT EXISTS matches on the name alone, so the
-- block ends with the same validity and definition check as that schema-migration, under
-- the same transaction-local search_path and quote_all_identifiers settings, put back
-- before it returns. To recover, follow ops/v1-lookahead-indexes/README.md.
DO $migration$
DECLARE
    checked_index text;
    expected_definition text;
    found_definition text;
    found_kind text;
    previous_search_path text;
    previous_quote_all_identifiers text;
BEGIN
    IF to_regclass('bigname_phase.normalized_events') IS NULL THEN
        RETURN;
    END IF;

    CREATE INDEX IF NOT EXISTS normalized_events_basenames_due_probe_idx
        ON bigname_phase.normalized_events (
            chain_id,
            (CASE WHEN jsonb_typeof(after_state -> 'expiry') IN ('number','string')
                AND after_state ->> 'expiry' ~ '^[+-]?[0-9]+$'
                AND length(ltrim(after_state ->> 'expiry', '+-0')) <= 19
              THEN ((CASE WHEN left(after_state ->> 'expiry', 1) = '-' THEN '-' ELSE '' END)
                || COALESCE(NULLIF(ltrim(after_state ->> 'expiry', '+-0'), ''), '0'))::numeric
            END),
            block_number
        )
        WHERE canonicality_state IN ('canonical','safe','finalized')
          AND source_family = 'basenames_base_registrar'
          AND event_kind IN ('RegistrationGranted','RegistrationRenewed','TokenControlTransferred');

    CREATE INDEX IF NOT EXISTS normalized_events_basenames_direct_node_probe_idx
        ON bigname_phase.normalized_events (
            chain_id,
            (COALESCE(namespace || ':' || lower(COALESCE(after_state ->> 'child_node', after_state ->> 'namehash', after_state ->> 'node', after_state #>> '{grant_source,node}', after_state #>> '{revocation_source,node}')), logical_name_id)),
            block_number
        )
        WHERE canonicality_state IN ('canonical','safe','finalized')
          AND source_family LIKE 'basenames\_base\_%';

    -- Every name below is schema-qualified or lives in pg_catalog.
    previous_search_path := current_setting('search_path');
    PERFORM set_config('search_path', 'pg_catalog', true);
    -- The expected text below has no quoted identifiers.
    previous_quote_all_identifiers := current_setting('quote_all_identifiers');
    PERFORM set_config('quote_all_identifiers', 'off', true);

    FOR checked_index, expected_definition IN
        SELECT * FROM (VALUES
            ('normalized_events_basenames_due_probe_idx',
             $def$CREATE INDEX normalized_events_basenames_due_probe_idx ON bigname_phase.normalized_events USING btree (chain_id, (
CASE
    WHEN ((jsonb_typeof((after_state -> 'expiry'::text)) = ANY (ARRAY['number'::text, 'string'::text])) AND ((after_state ->> 'expiry'::text) ~ '^[+-]?[0-9]+$'::text) AND (length(ltrim((after_state ->> 'expiry'::text), '+-0'::text)) <= 19)) THEN ((
    CASE
        WHEN ("left"((after_state ->> 'expiry'::text), 1) = '-'::text) THEN '-'::text
        ELSE ''::text
    END || COALESCE(NULLIF(ltrim((after_state ->> 'expiry'::text), '+-0'::text), ''::text), '0'::text)))::numeric
    ELSE NULL::numeric
END), block_number) WHERE ((canonicality_state = ANY (ARRAY['canonical'::bigname_phase.canonicality_state, 'safe'::bigname_phase.canonicality_state, 'finalized'::bigname_phase.canonicality_state])) AND (source_family = 'basenames_base_registrar'::text) AND (event_kind = ANY (ARRAY['RegistrationGranted'::text, 'RegistrationRenewed'::text, 'TokenControlTransferred'::text])))$def$),
            ('normalized_events_basenames_direct_node_probe_idx',
             $def$CREATE INDEX normalized_events_basenames_direct_node_probe_idx ON bigname_phase.normalized_events USING btree (chain_id, COALESCE(((namespace || ':'::text) || lower(COALESCE((after_state ->> 'child_node'::text), (after_state ->> 'namehash'::text), (after_state ->> 'node'::text), (after_state #>> '{grant_source,node}'::text[]), (after_state #>> '{revocation_source,node}'::text[])))), logical_name_id), block_number) WHERE ((canonicality_state = ANY (ARRAY['canonical'::bigname_phase.canonicality_state, 'safe'::bigname_phase.canonicality_state, 'finalized'::bigname_phase.canonicality_state])) AND (source_family ~~ 'basenames\_base\_%'::text))$def$)
        ) AS reviewed(index_name, definition)
    LOOP
        SELECT CASE relkind
                   WHEN 'i' THEN 'index'
                   WHEN 'I' THEN 'partitioned index'
                   WHEN 'r' THEN 'table'
                   WHEN 'p' THEN 'partitioned table'
                   WHEN 'v' THEN 'view'
                   WHEN 'm' THEN 'materialized view'
                   WHEN 'S' THEN 'sequence'
                   WHEN 'f' THEN 'foreign table'
                   WHEN 'c' THEN 'composite type'
                   ELSE 'relation of kind ' || relkind::text
               END
        INTO found_kind
        FROM pg_class
        WHERE oid = to_regclass('bigname_phase.' || checked_index);
        IF found_kind IS NULL THEN
            RAISE EXCEPTION
                '% does not exist although bigname_phase.normalized_events does; build it with ops/v1-lookahead-indexes/install.sql as ops/v1-lookahead-indexes/README.md describes, then run the schema-migrations again',
                checked_index;
        END IF;
        IF found_kind <> 'index' THEN
            RAISE EXCEPTION
                'bigname_phase.% is a %, not an index, so the index was never built; remove or rename that relation, then run the schema-migrations again',
                checked_index, found_kind;
        END IF;

        IF NOT EXISTS (
            SELECT 1
            FROM pg_index
            WHERE indexrelid = to_regclass('bigname_phase.' || checked_index)
              AND indrelid = to_regclass('bigname_phase.normalized_events')
              AND indisvalid
              AND indisready
        ) THEN
            RAISE EXCEPTION
                '% exists but is not a valid and ready index on bigname_phase.normalized_events; follow the recovery steps in ops/v1-lookahead-indexes/README.md, then run the schema-migrations again',
                checked_index;
        END IF;

        SELECT pg_get_indexdef(indexrelid)
        INTO found_definition
        FROM pg_index
        WHERE indexrelid = to_regclass('bigname_phase.' || checked_index);
        IF found_definition <> expected_definition THEN
            RAISE EXCEPTION
                '% exists but does not have the reviewed definition; found "%", expected "%"; follow the recovery steps in ops/v1-lookahead-indexes/README.md, then run the schema-migrations again',
                checked_index, found_definition, expected_definition;
        END IF;
    END LOOP;

    PERFORM set_config('search_path', previous_search_path, true);
    PERFORM set_config('quote_all_identifiers', previous_quote_all_identifiers, true);
END
$migration$;
