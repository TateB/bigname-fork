//! The RPC chain check for lookup endpoints: does an endpoint serve the chain it is configured for?

use std::fmt;

use bigname_domain::vocabulary::ChainId;
use serde_json::Value;

use crate::rpc::{ChainRpcUrls, JsonRpcHttpClient};

/// Why a configured endpoint failed the RPC chain check.
#[derive(Debug)]
pub enum RpcChainCheckError {
    /// The endpoint answered for another chain, or names a chain with no known chain id.
    Refused(String),
    /// The endpoint could not be asked. The text names the failure class, never the URL.
    Unreachable(String),
}

impl fmt::Display for RpcChainCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(message) | Self::Unreachable(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RpcChainCheckError {}

/// Compares `eth_chainId` and, with `check_genesis`, the block 0 hash of the endpoint configured
/// for `chain` against the chain's pinned identity. A chain with no pinned genesis still needs a
/// readable block 0 when `check_genesis` is set.
pub async fn verify_chain_rpc_url(
    rpc_urls: &ChainRpcUrls,
    chain: &str,
    check_genesis: bool,
) -> Result<(), RpcChainCheckError> {
    let parsed = chain.parse::<ChainId>().ok();
    let Some(expected_chain_id) = parsed.and_then(ChainId::numeric_chain_id) else {
        return Err(RpcChainCheckError::Refused(format!(
            "chain {chain} has no known EIP-155 chain id, so its RPC endpoint cannot be checked"
        )));
    };
    let endpoint = rpc_urls.url_for(chain).ok_or_else(|| {
        RpcChainCheckError::Refused(format!("chain {chain} has no configured RPC endpoint"))
    })?;
    let client = JsonRpcHttpClient::new_for_rpc_urls(endpoint, rpc_urls).map_err(|_| {
        RpcChainCheckError::Refused(format!(
            "the RPC endpoint for chain {chain} is not a valid URL"
        ))
    })?;
    let observed_chain_id = read(&client, chain, "eth_chainId", Vec::new())
        .await?
        .and_then(|value| u64::from_str_radix(value.as_str()?.strip_prefix("0x")?, 16).ok());
    let expected_genesis = parsed.and_then(ChainId::genesis_hash);
    let observed_genesis = if check_genesis {
        read(
            &client,
            chain,
            "eth_getBlockByNumber",
            vec![Value::String("0x0".to_owned()), Value::Bool(false)],
        )
        .await?
        .and_then(|block| Some(block.get("hash")?.as_str()?.to_ascii_lowercase()))
    } else {
        None
    };
    let genesis_matches = !check_genesis
        || match expected_genesis {
            Some(expected) => observed_genesis.as_deref() == Some(expected),
            None => observed_genesis.is_some(),
        };
    if observed_chain_id == Some(expected_chain_id) && genesis_matches {
        return Ok(());
    }
    let mut message = format!(
        "RPC endpoint for chain {chain} does not serve that chain: expected chain id \
         {expected_chain_id}, observed {}",
        observed_chain_id.map_or_else(|| "an unreadable value".to_owned(), |id| id.to_string())
    );
    if check_genesis && let Some(expected) = expected_genesis {
        message.push_str(&format!(
            "; expected genesis block hash {expected}, observed {}",
            observed_genesis.as_deref().unwrap_or("no block 0")
        ));
    }
    Err(RpcChainCheckError::Refused(message))
}

/// A JSON-RPC error answer reads as no value; only a failed exchange is unreachable.
async fn read(
    client: &JsonRpcHttpClient,
    chain: &str,
    method: &str,
    params: Vec<Value>,
) -> Result<Option<Value>, RpcChainCheckError> {
    match client.call(method, params).await {
        Ok(response) => Ok(response.result.ok().filter(|value| !value.is_null())),
        Err(error) => {
            let class = if error.chain().any(|cause| {
                cause
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_timeout)
            }) {
                "timed out"
            } else {
                "failed"
            };
            Err(RpcChainCheckError::Unreachable(format!(
                "{method} to the RPC endpoint for chain {chain} {class}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const SEPOLIA_GENESIS: &str =
        "0x25a5cc106eea7138acab33231d7160d69cb777ee0c2c553fcddf5138993e6dd9";

    /// Answers `eth_chainId` with `chain_id` and block 0 with `genesis` until dropped.
    async fn node(chain_id: &'static str, genesis: Option<&'static str>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let body = loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        buffer.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&buffer).into_owned();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if body.len() >= length {
                                break body.to_owned();
                            }
                        }
                    };
                    let call: Value = serde_json::from_str(&body).unwrap();
                    let result = match call["method"].as_str() {
                        Some("eth_chainId") => json!(chain_id),
                        Some("eth_getBlockByNumber") => {
                            genesis.map_or(Value::Null, |hash| json!({"hash": hash}))
                        }
                        _ => Value::Null,
                    };
                    let payload =
                        json!({"jsonrpc": "2.0", "id": call["id"], "result": result}).to_string();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    socket.write_all(reply.as_bytes()).await.unwrap();
                });
            }
        });
        format!("http://{address}/secret-key")
    }

    async fn check(chain: &str, url: &str, check_genesis: bool) -> Result<(), RpcChainCheckError> {
        let urls = ChainRpcUrls::from_entries(&[format!("{chain}={url}")]).unwrap();
        verify_chain_rpc_url(&urls, chain, check_genesis).await
    }

    #[tokio::test]
    async fn matching_endpoints_pass_and_other_chains_are_refused_without_the_url() {
        let sepolia = node("0xaa36a7", Some(SEPOLIA_GENESIS)).await;
        check("ethereum-sepolia", &sepolia, true).await.unwrap();

        let Err(RpcChainCheckError::Refused(message)) =
            check("ethereum-mainnet", &sepolia, true).await
        else {
            panic!("a Sepolia endpoint must not pass as Mainnet");
        };
        assert!(
            message.contains("expected chain id 1, observed 11155111"),
            "{message}"
        );
        assert!(message.contains(SEPOLIA_GENESIS), "{message}");
        assert!(!message.contains("secret-key") && !message.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn the_genesis_is_compared_only_when_asked() {
        let forked = node("0x1", Some("0x01")).await;
        assert!(matches!(
            check("ethereum-mainnet", &forked, true).await,
            Err(RpcChainCheckError::Refused(_))
        ));
        check("ethereum-mainnet", &forked, false).await.unwrap();

        let no_block_zero = node("0x2105", None).await;
        assert!(matches!(
            check("base-mainnet", &no_block_zero, true).await,
            Err(RpcChainCheckError::Refused(_))
        ));
        check("base-mainnet", &no_block_zero, false).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_chains_are_refused_and_dead_endpoints_are_unreachable() {
        let mainnet = node("0x1", None).await;
        assert!(matches!(
            check("base-sepolia", &mainnet, false).await,
            Err(RpcChainCheckError::Refused(_))
        ));
        let Err(RpcChainCheckError::Unreachable(message)) =
            check("ethereum-mainnet", "http://127.0.0.1:1/secret-key", false).await
        else {
            panic!("a closed port is unreachable");
        };
        assert!(!message.contains("secret-key"), "{message}");
    }
}
