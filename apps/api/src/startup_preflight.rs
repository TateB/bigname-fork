use anyhow::{Context, Result, bail};
use bigname_lookup::{ChainRpcUrls, RpcChainCheckError, verify_chain_rpc_url};
use sqlx::PgPool;

pub(crate) async fn ensure_verified_lookup_ddl_available(pool: &PgPool) -> Result<()> {
    let phase_schema_exists = bigname_storage::phase_schema_exists(pool)
        .await
        .context("API verified-lookup DDL preflight could not inspect the phase schema")?;
    if !phase_schema_exists {
        return Ok(());
    }

    let missing_ddl = bigname_storage::load_missing_api_lookup_ddl(pool)
        .await
        .context("API verified-lookup DDL preflight could not inspect required lookup DDL")?;
    if !missing_ddl.is_empty() {
        let diagnostics = missing_ddl
            .iter()
            .map(|object| format!("{}: {}", object.kind.as_str(), object.identity))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "API verified-lookup DDL preflight failed: required lookup objects are missing or serving relations are unreadable\n{diagnostics}"
        );
    }

    Ok(())
}

/// Refuses to serve when a configured RPC endpoint answers for another chain. An endpoint that
/// cannot be reached only warns, as lookups on that chain already fail on their own until it
/// answers.
pub(crate) async fn ensure_rpc_chains(urls: &ChainRpcUrls, check_genesis: bool) -> Result<()> {
    for (chain, _) in urls.iter() {
        match verify_chain_rpc_url(urls, chain, check_genesis).await {
            Ok(()) => {}
            Err(RpcChainCheckError::Refused(message)) => {
                bail!("API RPC chain check refused to start: {message}")
            }
            Err(RpcChainCheckError::Unreachable(message)) => tracing::warn!(
                service = "api",
                chain,
                error = %message,
                "API RPC chain check could not reach the endpoint; lookups on this chain fail until it answers"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "startup_preflight_tests.rs"]
mod tests;
