//! The runner's startup RPC chain check against a real endpoint, and the identity it records on
//! each ingest cursor.
#[path = "support/sepolia_rpc.rs"]
mod sepolia_rpc;
#[allow(dead_code)]
mod support;

use anyhow::{Result, ensure};
use bigname_ingest::RpcChainCheck;
use bigname_lookup::ChainRpcUrls;
use phase_runner::{
    config::{ChainConfig, SeedBasis, SourceConfig},
    error::ErrorKind,
    rpc_chain_check,
};
use sepolia_rpc::SepoliaIdentityRpc;
use support::ScratchDatabase;

const SEPOLIA_GENESIS: &str = "0x25a5cc106eea7138acab33231d7160d69cb777ee0c2c553fcddf5138993e6dd9";

fn keyed(rpc: &SepoliaIdentityRpc) -> String {
    rpc.endpoint.replace("http://", "http://user:secret@")
}

fn chain(chain_id: &str, endpoint: &str) -> Result<ChainConfig> {
    Ok(ChainConfig::new(
        chain_id,
        vec![SourceConfig::new(
            chain_id,
            "primary",
            "drpc",
            SeedBasis::EthereumHead,
            0,
            endpoint,
        )?],
        false,
    )?)
}

#[tokio::test]
async fn an_endpoint_on_another_chain_refuses_the_start_without_naming_its_url() -> Result<()> {
    let rpc = SepoliaIdentityRpc::start().await?;
    let mainnet = chain("ethereum-mainnet", &keyed(&rpc))?;

    let error = rpc_chain_check::verify_all(
        mainnet.sources.iter(),
        &ChainRpcUrls::default(),
        RpcChainCheck::Full,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Configuration);
    let rendered = error.to_string();
    for needle in ["ethereum-mainnet", "primary", "11155111"] {
        ensure!(rendered.contains(needle), "{rendered} lacks {needle}");
    }
    for secret in ["secret", "127.0.0.1"] {
        ensure!(!rendered.contains(secret), "{rendered} leaks {secret}");
    }

    let hydration =
        ChainRpcUrls::from_comma_delimited(&format!("ethereum-mainnet={}", keyed(&rpc)))?;
    let error = rpc_chain_check::verify_all([], &hydration, RpcChainCheck::ChainIdOnly)
        .await
        .unwrap_err();
    ensure!(error.to_string().contains("hydration"), "{error}");
    Ok(())
}

#[tokio::test]
async fn a_cursor_records_the_verified_identity_and_refuses_another_chain() -> Result<()> {
    let scratch = ScratchDatabase::create("bigname_rpc_identity").await?;
    let rpc = SepoliaIdentityRpc::start().await?;
    let mut chains = vec![chain("ethereum-sepolia", &keyed(&rpc))?];
    sqlx::query(
        "INSERT INTO ingest_cursors (
             chain_id, source_key, source_kind, seed_basis, start_block_number, next_block_number
         ) VALUES ('ethereum-sepolia', 'primary', 'rpc', 'ethereum_head', 0, 0)",
    )
    .execute(scratch.pool())
    .await?;
    let recorded = || async {
        sqlx::query_as::<_, (Option<i64>, Option<String>)>(
            "SELECT verified_chain_id, verified_genesis_hash FROM ingest_cursors
             WHERE chain_id = 'ethereum-sepolia' AND source_key = 'primary'",
        )
        .fetch_one(scratch.pool())
        .await
    };

    let verified = rpc_chain_check::verify_all(
        chains[0].sources.iter(),
        &ChainRpcUrls::default(),
        RpcChainCheck::ChainIdOnly,
    )
    .await?;
    rpc_chain_check::record_on_chains(&mut chains, &verified)?;
    rpc_chain_check::persist(scratch.pool(), &chains).await?;
    assert_eq!(recorded().await?, (Some(11_155_111), None));

    let verified = rpc_chain_check::verify_all(
        chains[0].sources.iter(),
        &ChainRpcUrls::default(),
        RpcChainCheck::Full,
    )
    .await?;
    rpc_chain_check::record_on_chains(&mut chains, &verified)?;
    rpc_chain_check::persist(scratch.pool(), &chains).await?;
    let full = (Some(11_155_111), Some(SEPOLIA_GENESIS.to_owned()));
    assert_eq!(recorded().await?, full, "a full check fills in the genesis");

    let mut sources = chains[0].sources.to_vec();
    sources[0].verified_rpc_chain = Some(bigname_ingest::ObservedRpcChain {
        chain_id: Some(1),
        genesis_hash: None,
    });
    let moved = [ChainConfig::new("ethereum-sepolia", sources, false)?];
    let error = rpc_chain_check::persist(scratch.pool(), &moved)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DataIntegrity);
    assert_eq!(recorded().await?, full, "a refused start changes nothing");

    scratch.cleanup().await
}
