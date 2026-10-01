//! Per-batch ENSv1, ENSv2 and Basenames Base state loading: restore only the history of names
//! the batch touches, and the whole retained ENSv2 history.
//! Canonical history remains the sole durable state.
use bigname_adapters::schema_v2::{
    BatchInput, ManifestInput, StateCacheCapacity, UnloadedNames, V1BatchDependencies,
    V1NodeRequest, begin_schema_v2_adapter_restore_with_provenance, collect_v1_batch_dependencies,
    prepare_schema_v2_batch_lookahead, restore_schema_v2_lookahead_session,
    v1_lookahead_supports_family,
};
use sqlx::PgPool;

use super::{
    LoadedBatch, cache, lookahead_query, lookahead_query::OrderedEvent, manifests, migration,
    resume,
};
use crate::{FullStateReason, InterpretError, Result, StateLoader};

/// Either the batch restored by lookahead, or the reason this chain needs the full-state loader.
pub(crate) enum Attempt {
    Loaded(Box<LoadedBatch>),
    FullStateRequired(StateLoader),
}

pub(crate) async fn batch_input(
    pool: &PgPool,
    chain_id: &str,
    from_block: i64,
    to_block: i64,
    resume_marker: Option<(i64, &str)>,
    state_cache_capacity: StateCacheCapacity,
    statement_timeout: Option<std::num::NonZeroU32>,
) -> Result<Attempt> {
    let mut tx = pool.begin().await.map_err(|error| {
        InterpretError::database("failed to begin lookahead input snapshot", error)
    })?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            InterpretError::database("failed to configure lookahead input snapshot", error)
        })?;
    // Off unless the operator sets it: a legitimately large batch must not be killed.
    if let Some(timeout) = statement_timeout {
        sqlx::query(&format!(
            "SET LOCAL statement_timeout = '{}s'",
            timeout.get()
        ))
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            InterpretError::database("failed to bound lookahead database reads", error)
        })?;
    }
    super::validate_snapshot_resume_marker(&mut tx, chain_id, resume_marker).await?;
    let orphaning_epoch = cache::orphaning_epoch(&mut tx, chain_id).await?;
    let (manifests, provenance) = manifests::load(&mut tx, chain_id).await?;
    if manifests.is_empty() {
        return Err(InterpretError::configuration(format!(
            "chain {chain_id} has no active manifests for interpretation"
        )));
    }
    // Decided inside this snapshot, so the choice matches the manifests the batch would use
    // and the history the full-state loader would restore. Deprecated manifests count too:
    // the full-state loader restores their retained events, and lookahead reads none from a
    // family it does not cover.
    let other_families = lookahead_query::other_manifest_families(&mut tx, chain_id).await?;
    let reason = match full_state_reason(&manifests, &provenance) {
        Some(reason) => Some(reason),
        None => retained_family_reason(&mut tx, chain_id, from_block, &other_families).await?,
    };
    if let Some(reason) = reason {
        return Ok(Attempt::FullStateRequired(StateLoader::FullState {
            reason,
        }));
    }
    let discovery_rules = super::load_discovery_rules(&mut tx, chain_id).await?;
    let mut admissions = super::load_admissions(&mut tx, chain_id, from_block).await?;
    admissions.extend(migration::admissions(&mut tx, chain_id, from_block).await?);
    let blocks = super::load_blocks(&mut tx, chain_id, from_block, to_block).await?;
    let raw_logs = super::load_raw_logs(&mut tx, chain_id, from_block, to_block).await?;
    let predecessor = resume::predecessor_timestamp(&mut tx, chain_id, from_block).await?;
    let last_timestamp = blocks
        .last()
        .ok_or_else(|| InterpretError::data_integrity("lookahead batch has no canonical blocks"))?
        .block_timestamp;
    let input = BatchInput {
        chain_id: chain_id.to_owned(),
        manifests,
        discovery_rules,
        admissions,
        blocks,
        raw_logs,
        prior_events: Vec::new(),
    };
    let mut dependencies = collect_v1_batch_dependencies(&input, &provenance)
        .map_err(|error| invalid_dependencies("decode", error))?;
    validate_dependencies(&dependencies)?;
    for name in
        lookahead_query::due_names(&mut tx, chain_id, from_block, predecessor, last_timestamp)
            .await?
    {
        let (namespace, node) = name.split_once(':').ok_or_else(|| {
            InterpretError::data_integrity("lookahead expiry candidate has no namespace")
        })?;
        dependencies.nodes.insert(V1NodeRequest {
            namespace: namespace.to_owned(),
            node: node.to_owned(),
        });
    }
    // ENSv2 state is restored whole: the first round of each attempt also reads every retained
    // ENSv2 event. Every event is written under one of the chain's manifests, so a chain with
    // no ENSv2 manifest row skips that read.
    let has_v2_manifest = provenance
        .iter()
        .map(|manifest| manifest.source_family.as_str())
        .chain(other_families.iter().map(|(family, _)| family.as_str()))
        .any(|family| family.starts_with("ens_v2_"));
    let (prepared, restored_event_count) = loop {
        let prior = load_closure(
            &mut tx,
            chain_id,
            from_block,
            &mut dependencies,
            has_v2_manifest,
        )
        .await?;
        let restored_event_count = prior.len();
        let restore = begin_schema_v2_adapter_restore_with_provenance(
            chain_id.to_owned(),
            input.manifests.clone(),
            provenance.clone(),
            input.discovery_rules.clone(),
            input.admissions.clone(),
            state_cache_capacity,
        )
        .map_err(|error| invalid_dependencies("begin restore", error))?;
        // Restore and interpretation both run under the loaded-names check. An attempt that
        // reads a name whose history was not loaded is discarded, and the next one loads it:
        // ENSv2 derives names from registry state while it interprets, so the collector cannot
        // name them all in advance. Every attempt that continues adds a name not loaded
        // before, a name the batch's logs and the snapshot's finite history derive, so the
        // attempts end. A name read under another spelling than the loaded one fails instead.
        let attempt =
            restore_schema_v2_lookahead_session(restore, prior, predecessor, &dependencies.nodes)
                .and_then(|session| {
                    prepare_schema_v2_batch_lookahead(
                        input.clone(),
                        provenance.clone(),
                        session,
                        &dependencies.nodes,
                        state_cache_capacity,
                    )
                });
        match attempt {
            Ok(prepared) => break (prepared, restored_event_count),
            Err(error) => match error.downcast_ref::<UnloadedNames>() {
                Some(UnloadedNames(names)) if names.is_disjoint(&dependencies.nodes) => {
                    #[cfg(test)]
                    RETRIES.set(RETRIES.get() + 1);
                    dependencies.nodes.extend(names.iter().cloned());
                }
                _ => {
                    return Err(InterpretError::data_integrity(format!(
                        "hash-covered adapter interpretation failed: {error:#}"
                    )));
                }
            },
        }
    };
    tx.commit().await.map_err(|error| {
        InterpretError::database("failed to commit lookahead input snapshot", error)
    })?;
    Ok(Attempt::Loaded(Box::new(LoadedBatch {
        input,
        provenance_manifests: provenance,
        prior_cache: cache::freshly_loaded(orphaning_epoch),
        adapter_session: None,
        prepared: Some(Box::new(prepared)),
        restored_event_count,
        lookahead_nodes: Some(dependencies.nodes),
    })))
}

/// Load the events of the requested names and resources, add the names and resources those
/// events link to, and repeat until a round adds nothing. There is no round limit: every round
/// that continues adds at least one name or resource that occurs in the chain's stored history
/// before this batch, that history is finite and fixed inside this snapshot, and nothing is
/// ever removed, so the set stops growing after finitely many rounds. A subname many labels
/// deep costs one round per label, because each stored `NewOwner` links a name to its parent.
/// Returns the loaded events in restore order, the ENSv2 events included when `whole_v2`.
async fn load_closure(
    connection: &mut sqlx::PgConnection,
    chain_id: &str,
    from_block: i64,
    dependencies: &mut V1BatchDependencies,
    whole_v2: bool,
) -> Result<Vec<bigname_adapters::schema_v2::PriorEventInput>> {
    // `None` until the first round has read them.
    let mut v2_events: Option<Vec<OrderedEvent>> = (!whole_v2).then(Vec::new);
    loop {
        validate_dependencies(dependencies)?;
        let previous = (dependencies.nodes.len(), dependencies.resource_ids.len());
        let names: Vec<_> = dependencies
            .nodes
            .iter()
            .map(|request| format!("{}:{}", request.namespace, request.node))
            .collect();
        let resources: Vec<_> = dependencies.resource_ids.iter().copied().collect();
        let mut events = lookahead_query::ordered_events(
            connection,
            chain_id,
            from_block,
            &names,
            &resources,
            v2_events.is_none(),
        )
        .await?;
        if v2_events.is_none() {
            let (v2, other) = events
                .into_iter()
                .partition(|ordered| ordered.event.source_family.starts_with("ens_v2_"));
            v2_events = Some(v2);
            events = other;
            dependencies
                .include_prior_events(v2_events.iter().flatten().map(|ordered| &ordered.event))
                .map_err(|error| invalid_dependencies("expand prior links", error))?;
        }
        dependencies
            .include_prior_events(events.iter().map(|ordered| &ordered.event))
            .map_err(|error| invalid_dependencies("expand prior links", error))?;
        dependencies.include_registry_only_resources(chain_id);
        validate_dependencies(dependencies)?;
        if previous == (dependencies.nodes.len(), dependencies.resource_ids.len()) {
            events.extend(v2_events.into_iter().flatten());
            events.sort_by_key(|ordered| ordered.order);
            return Ok(events.into_iter().map(|ordered| ordered.event).collect());
        }
        // Discard this partial fetch before querying the expanded set. Only the ENSv2 events,
        // which no later round reads again, are kept between rounds.
    }
}

/// The first manifest, in the loader's stable order, whose source family lookahead does not
/// cover. `provenance` holds the active and the deprecated manifests, the rows
/// `manifests::load` returns; `retained_family_reason` covers the other rollout states.
fn full_state_reason(
    active: &[ManifestInput],
    provenance: &[ManifestInput],
) -> Option<FullStateReason> {
    let unsupported = |manifests: &[ManifestInput], rollout_status| {
        manifests
            .iter()
            .find(|manifest| !v1_lookahead_supports_family(&manifest.source_family))
            .map(|manifest| FullStateReason::UnsupportedSourceFamily {
                source_family: manifest.source_family.clone(),
                rollout_status,
            })
    };
    unsupported(active, "active").or_else(|| unsupported(provenance, "deprecated"))
}

/// The first uncovered source family, in name order, whose manifest on the chain is in a
/// rollout state other than `active` or `deprecated` (`draft` or `shadow`) and that retains a
/// readable event before the batch. The full-state loader restores every retained event with
/// no source-family filter, while lookahead reads only `ens_v1_*`, `ens_v2_*` and
/// `basenames_base_*` families, so history of such a family (written while its manifest was
/// active, before the manifest moved back) would be restored by one loader and not the other.
///
/// Only families with a manifest row on the chain are probed: every event is written under
/// one of the chain's manifests, and `manifest_versions` rows are never deleted, only moved
/// between rollout states. A chain whose manifests are all `active` or `deprecated`, or
/// whose other manifests are all covered, runs no event query here.
async fn retained_family_reason(
    connection: &mut sqlx::PgConnection,
    chain_id: &str,
    from_block: i64,
    other_families: &[(String, String)],
) -> Result<Option<FullStateReason>> {
    let candidates: Vec<&(String, String)> = other_families
        .iter()
        .filter(|(family, _)| !v1_lookahead_supports_family(family))
        .collect();
    let mut families: Vec<String> = candidates
        .iter()
        .map(|(family, _)| family.clone())
        .collect();
    families.dedup();
    let Some(source_family) =
        lookahead_query::first_retained_family(connection, chain_id, from_block, &families).await?
    else {
        return Ok(None);
    };
    // The first row for the family in (family, status) order: `draft` before `shadow`.
    let rollout_status = candidates
        .iter()
        .find(|(family, _)| *family == source_family)
        .map(|(_, status)| status.as_str());
    let rollout_status = match rollout_status {
        Some("draft") => "draft",
        Some("shadow") => "shadow",
        other => {
            return Err(InterpretError::data_integrity(format!(
                "retained source family {source_family} has manifest rollout status {other:?}"
            )));
        }
    };
    Ok(Some(FullStateReason::UnsupportedSourceFamily {
        source_family,
        rollout_status,
    }))
}

fn validate_dependencies(dependencies: &V1BatchDependencies) -> Result<()> {
    if !dependencies.unsupported.is_empty() {
        // The loader was chosen because every manifest family is covered, and the prior-event
        // query reads only the covered families, so this is a broken invariant, not configuration.
        return Err(InterpretError::data_integrity(format!(
            "lookahead was chosen but does not cover: {:?}",
            dependencies.unsupported,
        )));
    }
    Ok(())
}

fn invalid_dependencies(operation: &str, error: anyhow::Error) -> InterpretError {
    InterpretError::data_integrity(format!("lookahead {operation} failed: {error:#}"))
}

#[cfg(test)]
thread_local! {
    /// Lookahead attempts discarded because they read an unloaded name, on this thread.
    pub(super) static RETRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[path = "lookahead_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "lookahead_equivalence_tests.rs"]
mod equivalence_tests;

#[cfg(test)]
#[path = "lookahead_basenames_equivalence_tests.rs"]
mod basenames_equivalence_tests;

#[cfg(test)]
#[path = "lookahead_ensv2_equivalence_tests.rs"]
mod ensv2_equivalence_tests;
