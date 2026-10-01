use bigname_storage::NameCurrentRow;

use super::{
    SnapshotReadResource, V2Error, V2Result, name_rows_error, v2_exact_name_snapshot_scope,
};
use crate::{AppState, v2::support::NormalizedRouteNameInput};

/// The name a name-shaped route reads for `input` in `namespace`: `input` itself, or for a
/// spelling with a bracketed labelhash, the name of the node's composed row. With no row the
/// node is not found once the namespace's family publication is servable; the bracketed text is
/// never read as a name of its own.
pub(super) async fn route_name(
    state: &AppState,
    namespace: &str,
    input: NormalizedRouteNameInput,
) -> V2Result<NormalizedRouteNameInput> {
    if input.node.is_none() {
        return Ok(input);
    }
    if let Some(row) = node_row(state, namespace, &input).await? {
        return Ok(named(input, row));
    }
    // As for a plain name: only a servable publication can say the node has no row.
    let chains: Vec<String> = v2_exact_name_snapshot_scope(state, namespace, None)
        .await?
        .required_positions()
        .iter()
        .map(|requirement| requirement.chain_id.clone())
        .collect();
    bigname_storage::families::name::ensure_family_publications(&state.pool, &chains)
        .await
        .map_err(name_rows_error(SnapshotReadResource::Name, |_| {
            V2Error::internal_error("failed to read the family publication")
        }))?;
    Err(V2Error::not_found(format!(
        "name {} was not found in namespace {namespace}",
        input.normalized_name
    )))
}

/// [`route_name`] for a continuation, which does not look the name up again: the node's row
/// only supplies its name, and without one the node is read by its bracketed spelling.
pub(super) async fn continuation_route_name(
    state: &AppState,
    namespace: &str,
    input: NormalizedRouteNameInput,
) -> V2Result<NormalizedRouteNameInput> {
    if input.node.is_none() {
        return Ok(input);
    }
    Ok(match node_row(state, namespace, &input).await? {
        Some(row) => named(input, row),
        None => input,
    })
}

/// The node's composed row when its name hashes back to the node: a surface whose raw label is
/// not its normalized form names another node by its text.
async fn node_row(
    state: &AppState,
    namespace: &str,
    input: &NormalizedRouteNameInput,
) -> V2Result<Option<NameCurrentRow>> {
    let logical_name_id = input.logical_name_id(namespace);
    let row = bigname_storage::load_name_current(&state.pool, &logical_name_id)
        .await
        .map_err(name_rows_error(SnapshotReadResource::Name, |_| {
            V2Error::internal_error(format!(
                "failed to load {}/{}",
                namespace, input.normalized_name
            ))
        }))?;
    Ok(row.filter(|row| {
        bigname_storage::logical_name_id_for_name(namespace, &row.normalized_name)
            == logical_name_id
    }))
}

fn named(input: NormalizedRouteNameInput, row: NameCurrentRow) -> NormalizedRouteNameInput {
    NormalizedRouteNameInput {
        normalized_name: row.normalized_name,
        node: None,
        ..input
    }
}
