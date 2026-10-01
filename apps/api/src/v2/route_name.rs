use sqlx::PgPool;

use super::{SnapshotReadResource, V2Error, V2Result, name_rows_error};
use crate::v2::support::NormalizedRouteNameInput;

/// The name a name-shaped route reads for `input` in `namespace`: `input` itself, or for a
/// spelling with a bracketed labelhash, the name of the node's composed row. With no row the
/// node is not found; the bracketed text is never read as a name of its own.
pub(super) async fn route_name(
    pool: &PgPool,
    namespace: &str,
    input: NormalizedRouteNameInput,
) -> V2Result<NormalizedRouteNameInput> {
    if input.node.is_none() {
        return Ok(input);
    }
    let logical_name_id = input.logical_name_id(namespace);
    let row = bigname_storage::load_name_current(pool, &logical_name_id)
        .await
        .map_err(name_rows_error(SnapshotReadResource::Name, |_| {
            V2Error::internal_error(format!(
                "failed to load {}/{}",
                namespace, input.normalized_name
            ))
        }))?;
    // A surface whose raw label is not its normalized form names another node by its text.
    match row {
        Some(row)
            if bigname_storage::logical_name_id_for_name(namespace, &row.normalized_name)
                == logical_name_id =>
        {
            Ok(NormalizedRouteNameInput {
                normalized_name: row.normalized_name,
                node: None,
                ..input
            })
        }
        _ => Err(V2Error::not_found(format!(
            "name {} was not found in namespace {namespace}",
            input.normalized_name
        ))),
    }
}
