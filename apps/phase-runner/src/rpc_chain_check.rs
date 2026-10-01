//! The startup RPC chain check: before anything durable happens, every configured RPC endpoint
//! must report the chain it is configured for.

use std::collections::BTreeMap;

use bigname_ingest::{
    ExpectedRpcChain, ObservedRpcChain, ProviderKind, RpcChainCheck, normalized_kind,
    verify_rpc_chain,
};
use bigname_lookup::ChainRpcUrls;
use sqlx::PgPool;
use tokio::task::JoinSet;

use crate::{
    config::{CapacityConfig, ChainConfig, SourceConfig},
    error::{ErrorKind, RunnerError, RunnerResult},
    metrics::RunnerMetricsFeed,
};

/// Source label the hydration RPC URL is checked and reported under.
pub const HYDRATION_SOURCE: &str = "hydration";

/// The mode the CLI set; a configuration built without one checks in full.
pub fn mode(capacity: &CapacityConfig) -> RpcChainCheck {
    capacity.ingest.rpc_chain_check().unwrap_or_default()
}

/// One endpoint that passed the check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedRpcEndpoint {
    pub chain: String,
    pub source_key: String,
    pub observed: ObservedRpcChain,
}

/// Checks every RPC source of every chain, whatever its role, and every hydration URL. Direct
/// Reth DB sources compare their stored genesis when they open, and Coinbase SQL is not an RPC
/// endpoint. Any failure refuses the start; the error and log name the chain, source and both
/// identities, never the URL.
pub async fn verify_all<'a>(
    sources: impl IntoIterator<Item = &'a SourceConfig>,
    hydration: &ChainRpcUrls,
    mode: RpcChainCheck,
) -> RunnerResult<Vec<VerifiedRpcEndpoint>> {
    let mut checks = JoinSet::new();
    let sources = sources
        .into_iter()
        .filter(|source| normalized_kind(&source.source_kind) == ProviderKind::Rpc)
        .map(|source| {
            (
                source.chain_id.clone(),
                source.source_key.clone(),
                source.endpoint().to_owned(),
            )
        });
    let hydration = hydration.iter().map(|(chain, url)| {
        (
            chain.to_owned(),
            HYDRATION_SOURCE.to_owned(),
            url.to_owned(),
        )
    });
    for (chain, source_key, endpoint) in sources.chain(hydration) {
        let expected = ExpectedRpcChain::new(&chain, &source_key, mode)
            .map_err(|error| RunnerError::new(ErrorKind::Configuration, error.to_string()))?;
        checks.spawn(async move {
            let result = verify_rpc_chain(&endpoint, &expected).await;
            (expected, result)
        });
    }
    let mut verified = Vec::new();
    let mut first_failure = None;
    while let Some(joined) = checks.join_next().await {
        let (expected, result) = joined.map_err(|error| {
            RunnerError::transient(format!("RPC chain check task failed: {error}"))
        })?;
        match result {
            Ok(observed) => verified.push(VerifiedRpcEndpoint {
                chain: expected.chain().to_owned(),
                source_key: expected.source_key().to_owned(),
                observed,
            }),
            Err(error) => {
                match error.rpc_chain_mismatch() {
                    Some(mismatch) => tracing::error!(
                        chain = mismatch.chain,
                        source = mismatch.source_key,
                        expected_chain_id = mismatch.expected_chain_id,
                        observed_chain_id = mismatch.observed_chain_id,
                        expected_genesis_hash = mismatch.expected_genesis_hash,
                        observed_genesis_hash = mismatch.observed_genesis_hash,
                        "RPC endpoint does not serve its configured chain; refusing to start"
                    ),
                    None => tracing::error!(
                        chain = expected.chain(),
                        source = expected.source_key(),
                        error = %error,
                        "RPC chain check could not complete; refusing to start"
                    ),
                }
                first_failure.get_or_insert(RunnerError::new(
                    ErrorKind::Configuration,
                    format!("refusing to start: {error}"),
                ));
            }
        }
    }
    match first_failure {
        Some(error) => Err(error),
        None => {
            verified.sort_by(|left, right| {
                (&left.chain, &left.source_key).cmp(&(&right.chain, &right.source_key))
            });
            Ok(verified)
        }
    }
}

/// Copies what each source's endpoint reported onto the source, so a new ingest cursor is
/// created with it.
pub fn record_on_chains(
    chains: &mut [ChainConfig],
    verified: &[VerifiedRpcEndpoint],
) -> RunnerResult<()> {
    let observed = verified
        .iter()
        .map(|endpoint| {
            (
                (endpoint.chain.as_str(), endpoint.source_key.as_str()),
                &endpoint.observed,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for chain in chains {
        let sources = chain
            .sources
            .iter()
            .map(|source| {
                let mut source = source.clone();
                source.verified_rpc_chain = observed
                    .get(&(source.chain_id.as_str(), source.source_key.as_str()))
                    .map(|observed| (*observed).clone());
                source
            })
            .collect();
        *chain = ChainConfig::new(chain.chain_id.clone(), sources, chain.verify_before_live)?;
    }
    Ok(())
}

pub fn report(feed: &RunnerMetricsFeed, verified: &[VerifiedRpcEndpoint]) {
    for endpoint in verified {
        feed.rpc_chain_verified(
            &endpoint.chain,
            &endpoint.source_key,
            endpoint.observed.chain_id,
        );
    }
}

/// Persists each intake cursor's verified identity on first verified start and refuses a start
/// whose endpoint reports a different chain id, or in full mode a different genesis, than the
/// cursor recorded.
pub async fn persist(pool: &PgPool, chains: &[ChainConfig]) -> RunnerResult<()> {
    for source in chains
        .iter()
        .flat_map(|chain| chain.intake_sources().to_vec())
    {
        crate::ingest_cursor_config::record_verified_rpc_chain(pool, &source).await?;
    }
    Ok(())
}
