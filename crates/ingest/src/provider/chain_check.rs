use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use bigname_domain::vocabulary::ChainId;
use serde_json::Value;
use tokio::{sync::Mutex, time::Instant};

use super::{JsonRpcProvider, decode::hash_hex_from_str};
use crate::IngestError;

/// How long a verified endpoint is trusted before the next request checks it again.
pub const RPC_CHAIN_RECHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// What the RPC chain check compares. There is deliberately no mode that skips it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RpcChainCheck {
    /// `eth_chainId`, plus the block 0 hash against the pinned genesis where one is pinned.
    #[default]
    Full,
    /// `eth_chainId` only, for local nodes that run under a production chain's id.
    ChainIdOnly,
}

impl RpcChainCheck {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ChainIdOnly => "chain-id-only",
        }
    }
}

impl FromStr for RpcChainCheck {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "full" => Ok(Self::Full),
            "chain-id-only" => Ok(Self::ChainIdOnly),
            _ => Err(format!(
                "unknown RPC chain check {value:?}; expected full or chain-id-only"
            )),
        }
    }
}

/// The identity one configured endpoint must report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedRpcChain {
    chain: String,
    source_key: String,
    chain_id: u64,
    genesis_hash: Option<&'static str>,
    mode: RpcChainCheck,
}

impl ExpectedRpcChain {
    /// Fails closed for a chain slug with no known numeric chain id.
    pub fn new(chain: &str, source_key: &str, mode: RpcChainCheck) -> crate::Result<Self> {
        let chain_id = chain
            .parse::<ChainId>()
            .ok()
            .and_then(ChainId::numeric_chain_id)
            .ok_or_else(|| {
                IngestError::configuration(format!(
                    "chain {chain} source {source_key} has no known EIP-155 chain id, so its \
                     RPC endpoint cannot be checked"
                ))
            })?;
        let genesis_hash = chain
            .parse::<ChainId>()
            .ok()
            .and_then(ChainId::genesis_hash);
        Ok(Self {
            chain: chain.to_owned(),
            source_key: source_key.to_owned(),
            chain_id,
            genesis_hash,
            mode,
        })
    }

    pub fn chain(&self) -> &str {
        &self.chain
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }

    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub const fn mode(&self) -> RpcChainCheck {
        self.mode
    }

    fn mismatch(&self, observed: &ObservedRpcChain) -> RpcChainMismatch {
        RpcChainMismatch {
            chain: self.chain.clone(),
            source_key: self.source_key.clone(),
            expected_chain_id: self.chain_id,
            observed_chain_id: observed.chain_id,
            expected_genesis_hash: self.genesis_hash.map(str::to_owned),
            observed_genesis_hash: observed.genesis_hash.clone(),
        }
    }
}

/// What an endpoint reported. The genesis hash is read only in [`RpcChainCheck::Full`] mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedRpcChain {
    pub chain_id: Option<u64>,
    pub genesis_hash: Option<String>,
}

/// An endpoint that does not serve the chain it is configured for. Neither field set nor
/// rendering ever includes the endpoint URL, which carries provider keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcChainMismatch {
    pub chain: String,
    pub source_key: String,
    pub expected_chain_id: u64,
    pub observed_chain_id: Option<u64>,
    pub expected_genesis_hash: Option<String>,
    pub observed_genesis_hash: Option<String>,
}

impl fmt::Display for RpcChainMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "RPC endpoint for chain {} source {} does not serve that chain: expected chain id {}, \
             observed {}",
            self.chain,
            self.source_key,
            self.expected_chain_id,
            display_or(
                self.observed_chain_id.map(|id| id.to_string()),
                "an unreadable value"
            )
        )?;
        if let Some(expected) = &self.expected_genesis_hash {
            write!(
                formatter,
                "; expected genesis block hash {expected}, observed {}",
                display_or(self.observed_genesis_hash.clone(), "no block 0")
            )?;
        }
        Ok(())
    }
}

fn display_or(value: Option<String>, missing: &str) -> String {
    value.unwrap_or_else(|| missing.to_owned())
}

impl std::error::Error for RpcChainMismatch {}

pub(super) fn chain_mismatch_in(error: &anyhow::Error) -> Option<&RpcChainMismatch> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RpcChainMismatch>())
}

/// The check one provider repeats: before its first request, after [`RPC_CHAIN_RECHECK_INTERVAL`],
/// and after its HTTP client was rebuilt.
pub(super) struct ChainGuard {
    expected: ExpectedRpcChain,
    recheck_after: Duration,
    verified: Mutex<Option<(Instant, u64)>>,
}

impl ChainGuard {
    pub(super) fn new(expected: ExpectedRpcChain, recheck_after: Duration) -> Self {
        Self {
            expected,
            recheck_after,
            verified: Mutex::new(None),
        }
    }
}

impl JsonRpcProvider {
    /// Reads the endpoint's chain id and, in full mode, its block 0 hash, and fails with a
    /// [`RpcChainMismatch`] unless they match `expected`. A missing block 0 or an unreadable
    /// chain id is a mismatch; a transport failure keeps its own (retried) error.
    pub async fn verify_chain(&self, expected: &ExpectedRpcChain) -> Result<ObservedRpcChain> {
        let chain_id = self
            .request_unchecked("eth_chainId", Vec::new())
            .await?
            .and_then(|value| parse_chain_id(&value));
        let genesis_hash = match expected.mode {
            RpcChainCheck::ChainIdOnly => None,
            RpcChainCheck::Full => self
                .request_unchecked(
                    "eth_getBlockByNumber",
                    vec![Value::String("0x0".to_owned()), Value::Bool(false)],
                )
                .await?
                .and_then(|block| {
                    let hash = block.get("hash")?.as_str()?;
                    hash_hex_from_str(hash, "genesis block hash").ok()
                }),
        };
        let observed = ObservedRpcChain {
            chain_id,
            genesis_hash,
        };
        let genesis_matches = match (expected.mode, expected.genesis_hash) {
            (RpcChainCheck::ChainIdOnly, _) => true,
            (RpcChainCheck::Full, Some(genesis)) => {
                observed.genesis_hash.as_deref() == Some(genesis)
            }
            (RpcChainCheck::Full, None) => observed.genesis_hash.is_some(),
        };
        if observed.chain_id != Some(expected.chain_id) || !genesis_matches {
            return Err(expected.mismatch(&observed).into());
        }
        Ok(observed)
    }

    pub(super) async fn ensure_chain(&self) -> Result<()> {
        let Some(guard) = &self.chain_guard else {
            return Ok(());
        };
        let mut verified = guard.verified.lock().await;
        let client_id = self.client.client_id();
        if verified.is_some_and(|(at, id)| id == client_id && at.elapsed() < guard.recheck_after) {
            return Ok(());
        }
        *verified = None;
        self.verify_chain(&guard.expected)
            .await
            .context("RPC chain check failed")?;
        *verified = Some((Instant::now(), self.client.client_id()));
        Ok(())
    }
}

fn parse_chain_id(value: &Value) -> Option<u64> {
    u64::from_str_radix(value.as_str()?.strip_prefix("0x")?, 16).ok()
}
