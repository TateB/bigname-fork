//! API-owned publication identity and response revalidation; storage owns all underlying reads.
use super::*;

impl PublicNamespaceSet {
    pub(crate) fn for_namespace(self, namespace: Option<&str>) -> Self {
        let Some(namespace) = namespace else {
            return self;
        };
        let deployments = self
            .deployments
            .iter()
            .filter(|entry| entry.namespace == namespace)
            .cloned()
            .collect::<Vec<_>>();
        let request_scope = self
            .request_scope
            .iter()
            .filter(|scope| {
                deployments
                    .iter()
                    .any(|deployment| deployment.scope == scope.scope)
            })
            .cloned()
            .collect();
        let manifests = self
            .manifest_tokens
            .iter()
            .filter(|entry| entry.namespace == namespace)
            .cloned()
            .collect();
        Self::new(deployments, request_scope, manifests)
    }

    /// Whether `conn` still serves every captured publication: each chain's family marker is
    /// the same generation at the same position. A read on `conn` then reads the captured
    /// publications.
    pub(crate) async fn served_on(&self, conn: &mut sqlx::PgConnection) -> ApiResult<bool> {
        for token in self
            .deployments
            .iter()
            .filter_map(|deployment| deployment.read_token.as_ref())
        {
            let current = load_selected_project_generations_on(&mut *conn, &token.selected, true)
                .await
                .map_err(|_| {
                    ApiError::internal_error("failed to validate public namespace data")
                })?;
            if current.as_ref() != Some(&token.project_generations) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The namespace authority the request was admitted under is still the configured one. The
/// publications need no recheck: the page read them on one snapshot that served them.
pub(crate) async fn revalidate_collection_manifests(
    state: &AppState,
    expected: &PublicNamespaceSet,
    namespace: Option<&str>,
) -> ApiResult<()> {
    if state.public_namespaces_override().is_some() {
        return Ok(());
    }
    let mut tokens = load_public_namespace_manifest_tokens(&state.pool).await?;
    tokens.retain(|token| namespace.is_none_or(|namespace| token.namespace == namespace));
    if expected.manifest_tokens.as_ref() != tokens.as_slice() {
        return Err(public_namespace_manifest_conflict());
    }
    Ok(())
}
