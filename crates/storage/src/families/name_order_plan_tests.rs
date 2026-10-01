//! Plan-shape tests for the readers that walk readable name surfaces in name order: the search
//! candidates, a resolver's bound-name candidates and the reverse lookup candidates.
//!
//! Search and bound names must be able to read `name_surfaces_name_order_idx` in page order, with
//! the keyset cursor as its index condition and no Sort, under either plan. The fixture is small,
//! so `enable_sort` is off to stand in for a large table: the assertions are about which access
//! paths the planner can use at all, not about costs. A `LIKE` prefix becomes an index range only
//! on a C-collated database, so it stays a filter on the ordered scan here.
//!
//! The reverse lookup keeps its cost-based plan: for an address with three names it must start
//! from `project_address_name_index` rather than walk every name in order. Its statement is never
//! prepared (`persistent(false)`), so it is always planned with its values and only the custom plan
//! is checked.
//!
//! Every walk returns the same rows in batches as in one sorted read, and none returns the name
//! longer than 2000 bytes, which inserts although the index leaves it out.

use anyhow::{Context, Result, ensure};
use sqlx::{PgConnection, raw_sql};

use super::{
    id_index_plan_tests::{PLAN_MODES, Probe, explain_execute, missing_probes, with_database},
    name::{BOUND_CANDIDATES_SQL, SEARCH_CANDIDATES_SQL},
    records::REVERSE_CANDIDATES_SQL,
};

const CHAIN: &str = "ethereum-sepolia";
const ROWS: i64 = 2_000;
const ORDER_INDEX: &str = "name_surfaces_name_order_idx";
const KEYSET: &str = "(ROW(raw_name, namespace, namehash) > ROW(";

type Candidate = (String, String, String, String);

/// A prepared reader and the values of one call, with `{after}` and `{limit}` left open.
struct Walk {
    label: &'static str,
    statement: &'static str,
    values: String,
}

#[tokio::test]
async fn name_ordered_walks_read_the_name_order_index() -> Result<()> {
    with_database("family_name_order_plan", async |connection| {
        install_fixture(connection).await?;
        prepare(connection).await?;
        check_ordered_plans(connection).await?;
        check_reverse_plan(connection).await?;
        check_walks(connection).await
    })
    .await
}

async fn prepare(connection: &mut PgConnection) -> Result<()> {
    for (label, types, sql) in [
        (
            "search_candidates",
            "text[], text, text, text, text, text, bigint",
            SEARCH_CANDIDATES_SQL,
        ),
        (
            "bound_candidates",
            "text, text, text, text, text, text, bigint",
            BOUND_CANDIDATES_SQL,
        ),
        (
            "reverse_candidates",
            "text, text[], jsonb, boolean, text[], text, text, text, bigint, text[]",
            REVERSE_CANDIDATES_SQL,
        ),
    ] {
        raw_sql(&format!("PREPARE {label} ({types}) AS {sql}"))
            .execute(&mut *connection)
            .await
            .with_context(|| format!("prepare {label}"))?;
    }
    Ok(())
}

fn walks() -> [Walk; 4] {
    [
        Walk {
            label: "search_candidates",
            statement: "search prefix",
            values: "'{ens}', NULL, 'a%', {after}, {limit}".to_owned(),
        },
        Walk {
            label: "search_candidates",
            statement: "search contains",
            values: "'{ens,basenames}', NULL, '%a%', {after}, {limit}".to_owned(),
        },
        Walk {
            label: "bound_candidates",
            statement: "bound names",
            values: format!("'{CHAIN}', '{}', NULL, {{after}}, {{limit}}", address('a')),
        },
        Walk {
            label: "reverse_candidates",
            statement: "reverse lookup",
            values: format!(
                "'{}', '{{ens,basenames}}', '{{}}', false, '{{registrant,token_holder}}', \
                 {{after}}, {{limit}}, NULL",
                address('d')
            ),
        },
    ]
}

async fn check_ordered_plans(connection: &mut PgConnection) -> Result<()> {
    raw_sql("SET enable_sort = off")
        .execute(&mut *connection)
        .await?;
    let probe = [Probe {
        index: ORDER_INDEX,
        conditions: &[KEYSET],
    }];
    let mut failures = Vec::new();
    for walk in walks()
        .iter()
        .filter(|walk| walk.label != "reverse_candidates")
    {
        // The first batch and a continuation both bind the cursor as the index condition.
        for after in [None, Some(("a", "ens", "0x"))] {
            let values = values(walk, after, 201);
            for mode in PLAN_MODES {
                let plan = explain_execute(connection, mode, walk.label, &values).await?;
                let label = format!("{} after {after:?} ({mode})", walk.statement);
                failures.extend(missing_probes(&label, &plan, &probe));
                if plan.iter().any(|line| line.contains("Sort")) {
                    failures.push(format!("{label}: sorts\n{}", plan.join("\n")));
                }
            }
        }
    }
    raw_sql("RESET enable_sort")
        .execute(&mut *connection)
        .await?;
    ensure!(
        failures.is_empty(),
        "name-ordered plans:\n{}",
        failures.join("\n\n")
    );
    Ok(())
}

async fn check_reverse_plan(connection: &mut PgConnection) -> Result<()> {
    let values = format!(
        "'{}', '{{ens,basenames}}', '{{}}', false, '{{registrant,token_holder}}', \
         NULL, NULL, NULL, 65, NULL",
        address('c')
    );
    let plan = explain_execute(
        connection,
        "force_custom_plan",
        "reverse_candidates",
        &values,
    )
    .await?;
    let mut failures = missing_probes(
        "reverse lookup",
        &plan,
        &[Probe {
            index: "project_address_name_index_pkey",
            conditions: &["(address = "],
        }],
    );
    if plan.iter().any(|line| line.contains(ORDER_INDEX)) {
        failures.push("reverse lookup: walks the name order index".to_owned());
    }
    ensure!(
        failures.is_empty(),
        "{}\n{}",
        failures.join("\n"),
        plan.join("\n")
    );
    Ok(())
}

async fn check_walks(connection: &mut PgConnection) -> Result<()> {
    let long: Vec<String> = sqlx::query_scalar(
        "SELECT logical_name_id FROM name_surfaces
         WHERE visibility_state = 'active' AND octet_length(raw_name) > 2000",
    )
    .fetch_all(&mut *connection)
    .await?;
    ensure!(long.len() == 1, "long names: {long:?}");
    for walk in walks() {
        let sorted = read(connection, &walk, None, ROWS * 2).await?;
        ensure!(
            sorted.len() > 20,
            "{}: {} rows",
            walk.statement,
            sorted.len()
        );
        ensure!(
            sorted.iter().all(|(id, ..)| *id != long[0]),
            "{}: returned the long name",
            walk.statement
        );
        raw_sql("SET enable_sort = off")
            .execute(&mut *connection)
            .await?;
        for mode in PLAN_MODES {
            raw_sql(&format!("SET plan_cache_mode = {mode}"))
                .execute(&mut *connection)
                .await?;
            let mut walked = Vec::new();
            loop {
                let after = walked
                    .last()
                    .map(|(_, name, namespace, namehash): &Candidate| {
                        (name.as_str(), namespace.as_str(), namehash.as_str())
                    });
                let batch = read(connection, &walk, after, 7).await?;
                let done = batch.len() < 7;
                walked.extend(batch);
                if done {
                    break;
                }
            }
            ensure!(
                walked == sorted,
                "{} ({mode}): the batched walk differs from the sorted read",
                walk.statement
            );
        }
        raw_sql("RESET enable_sort; RESET plan_cache_mode")
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

async fn read(
    connection: &mut PgConnection,
    walk: &Walk,
    after: Option<(&str, &str, &str)>,
    limit: i64,
) -> Result<Vec<Candidate>> {
    sqlx::query_as(&format!(
        "EXECUTE {} ({})",
        walk.label,
        values(walk, after, limit)
    ))
    .persistent(false)
    .fetch_all(&mut *connection)
    .await
    .with_context(|| walk.statement)
}

fn values(walk: &Walk, after: Option<(&str, &str, &str)>, limit: i64) -> String {
    let after = after.map_or_else(
        || "NULL, NULL, NULL".to_owned(),
        |(name, namespace, namehash)| format!("'{name}', '{namespace}', '{namehash}'"),
    );
    walk.values
        .replace("{after}", &after)
        .replace("{limit}", &limit.to_string())
}

fn address(fill: char) -> String {
    format!("0x{}", fill.to_string().repeat(40))
}

async fn install_fixture(connection: &mut PgConnection) -> Result<()> {
    // Name n is active unless n is a multiple of 50 and readable unless its block, every 70th, is
    // orphaned; even names are ens and odd names basenames. One more ens name is about 6.6 KB of
    // incompressible labels, past the btree entry limit. Resolver 0xbb.. names the first three
    // names and 0xaa.. all others; address 0xcc.. holds the first six and 0xdd.. all others.
    raw_sql(&format!(
        "INSERT INTO chain_lineage
             (chain_id, block_hash, block_number, block_timestamp, canonicality_state)
         SELECT '{CHAIN}', 'block-' || n, n, to_timestamp(n),
                (CASE WHEN n % 70 = 0 THEN 'orphaned' ELSE 'canonical' END)::canonicality_state
         FROM generate_series(1, {ROWS} + 1) n;

         INSERT INTO project_family_marker (chain_id, current_block_number, current_block_hash,
             state)
         VALUES ('{CHAIN}', {ROWS} + 1, 'block-' || ({ROWS} + 1), 'live');

         INSERT INTO name_surfaces (logical_name_id, namespace, raw_name, raw_labels,
             dns_encoded_name, namehash, labelhashes, normalizer_version, visibility_state,
             deactivation_reason, deactivated_at, chain_id, block_hash, block_number,
             canonicality_state)
         SELECT namespace || ':' || namehash, namespace, raw_name, ARRAY[raw_name], '\\x00',
                namehash, ARRAY[namehash], 'v1',
                CASE WHEN n % 50 = 0 THEN 'shadow' ELSE 'active' END,
                CASE WHEN n % 50 = 0 THEN 'invalid' END, CASE WHEN n % 50 = 0 THEN now() END,
                '{CHAIN}', 'block-' || n, n,
                (CASE WHEN n % 70 = 0 THEN 'orphaned' ELSE 'canonical' END)::canonicality_state
         FROM generate_series(1, {ROWS} + 1) n,
         LATERAL (
             SELECT CASE WHEN n % 2 = 0 THEN 'ens' ELSE 'basenames' END AS namespace,
                    '0x' || lpad(to_hex(n), 64, '0') AS namehash,
                    CASE WHEN n > {ROWS}
                         THEN (SELECT string_agg(md5(i::text), '.')
                               FROM generate_series(1, 200) i) || '.eth'
                         ELSE substr(md5(n::text), 1, 8)
                              || CASE WHEN n % 2 = 0 THEN '.eth' ELSE '.base.eth' END
                    END AS raw_name
         ) surface;

         INSERT INTO project_named_resource_pointer (chain_id, resource_id, logical_name_id,
             block_number, event_identity, resolver_address, source_family)
         SELECT chain_id, lpad(to_hex(block_number), 32, '0')::uuid, logical_name_id,
                block_number, 'pointer:' || block_number,
                '0x' || repeat(CASE WHEN block_number <= 3 THEN 'b' ELSE 'a' END, 40),
                'ens_v1_registry_l1'
         FROM name_surfaces;

         INSERT INTO project_address_name_index (address, logical_name_id, relation, chain_id)
         SELECT '0x' || repeat(CASE WHEN block_number <= 6 THEN 'c' ELSE 'd' END, 40),
                logical_name_id, 'registrant', chain_id
         FROM name_surfaces;

         ANALYZE"
    ))
    .execute(&mut *connection)
    .await?;
    Ok(())
}
