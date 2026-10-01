//! The RPC chain check for lookup endpoints: does an endpoint serve the chain it is configured for?

use std::fmt;

use bigname_domain::vocabulary::ChainId;
use serde_json::Value;

use crate::rpc::{ChainRpcUrls, JsonRpcHttpClient};

/// Why a configured endpoint failed the RPC chain check.
#[derive(Debug)]
pub enum RpcChainCheckError {
    /// The endpoint answered for another chain, or is not a valid endpoint.
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

/// Compares `eth_chainId` of the endpoint configured for `chain` with `expected_chain_id` and,
/// with `check_genesis`, its block 0 hash with the chain's pinned genesis. A chain with no pinned
/// genesis still needs a readable block 0 hash when `check_genesis` is set. A wrong chain id is
/// refused before block 0 is asked for, so a later transport failure cannot mask it.
pub async fn verify_chain_rpc_url(
    rpc_urls: &ChainRpcUrls,
    chain: &str,
    expected_chain_id: u64,
    check_genesis: bool,
) -> Result<(), RpcChainCheckError> {
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
    if observed_chain_id != Some(expected_chain_id) {
        return Err(RpcChainCheckError::Refused(format!(
            "RPC endpoint for chain {chain} does not serve that chain: expected chain id \
             {expected_chain_id}, observed {}",
            observed_chain_id.map_or_else(|| "an unreadable value".to_owned(), |id| id.to_string())
        )));
    }
    if !check_genesis {
        return Ok(());
    }
    let observed_genesis = read(
        &client,
        chain,
        "eth_getBlockByNumber",
        vec![Value::String("0x0".to_owned()), Value::Bool(false)],
    )
    .await?
    .and_then(|block| block_hash(block.get("hash")?.as_str()?));
    let expected_genesis = chain
        .parse::<ChainId>()
        .ok()
        .and_then(ChainId::genesis_hash);
    let genesis_matches = match expected_genesis {
        Some(expected) => observed_genesis.as_deref() == Some(expected),
        None => observed_genesis.is_some(),
    };
    if genesis_matches {
        return Ok(());
    }
    Err(RpcChainCheckError::Refused(format!(
        "RPC endpoint for chain {chain} does not serve that chain: expected genesis block hash \
         {}, observed {}",
        expected_genesis.unwrap_or("a readable block 0 hash"),
        observed_genesis.as_deref().unwrap_or("none")
    )))
}

/// A 32-byte hex hash in lowercase, or `None` for anything else.
fn block_hash(value: &str) -> Option<String> {
    let digits = value.strip_prefix("0x")?;
    (digits.len() == 64 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
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
        let expected = match chain {
            "ethereum-mainnet" => 1,
            "ethereum-sepolia" => 11_155_111,
            "base-mainnet" => 8453,
            "base-sepolia" => 84_532,
            _ => unreachable!("{chain}"),
        };
        verify_chain_rpc_url(&urls, chain, expected, check_genesis).await
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
        assert!(!message.contains("secret-key") && !message.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn a_wrong_chain_id_is_refused_before_block_zero_is_asked_for() {
        // Block 0 is never answered here, so asking for it would turn the refusal into a timeout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut chunk = [0_u8; 4096];
            let _ = socket.read(&mut chunk).await;
            let payload = json!({"jsonrpc": "2.0", "id": 1, "result": "0xaa36a7"}).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            let _held = listener.accept().await;
            std::future::pending::<()>().await;
        });
        let checked = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            check("ethereum-mainnet", &format!("http://{address}/"), true),
        )
        .await
        .expect("asking for block 0 would wait forever");
        let Err(RpcChainCheckError::Refused(message)) = checked else {
            panic!("a wrong chain id is a refusal, whatever block 0 does");
        };
        assert!(message.contains("observed 11155111"), "{message}");
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

        let malformed = node("0x2105", Some("garbage")).await;
        assert!(matches!(
            check("base-mainnet", &malformed, true).await,
            Err(RpcChainCheckError::Refused(_))
        ));
        let base = node("0x2105", Some(SEPOLIA_GENESIS)).await;
        check("base-mainnet", &base, true).await.unwrap();
        let base_sepolia = node("0x14a34", Some(SEPOLIA_GENESIS)).await;
        check("base-sepolia", &base_sepolia, true).await.unwrap();
    }

    #[tokio::test]
    async fn dead_endpoints_are_unreachable_without_the_url() {
        let Err(RpcChainCheckError::Unreachable(message)) =
            check("ethereum-mainnet", "http://127.0.0.1:1/secret-key", false).await
        else {
            panic!("a closed port is unreachable");
        };
        assert!(!message.contains("secret-key"), "{message}");
    }
}
