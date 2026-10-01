use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

use super::*;

const SEPOLIA_GENESIS: &str = "0x25a5cc106eea7138acab33231d7160d69cb777ee0c2c553fcddf5138993e6dd9";

async fn sepolia_node() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/{key}",
        post(|Json(call): Json<Value>| async move {
            let result = match call["method"].as_str() {
                Some("eth_chainId") => json!("0xaa36a7"),
                Some("eth_getBlockByNumber") => json!({"hash": SEPOLIA_GENESIS}),
                _ => Value::Null,
            };
            Json(json!({"jsonrpc": "2.0", "id": call["id"], "result": result}))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}/secret-key")
}

fn urls(entries: &[(&str, &str)]) -> ChainRpcUrls {
    let entries = entries
        .iter()
        .map(|(chain, url)| format!("{chain}={url}"))
        .collect::<Vec<_>>();
    ChainRpcUrls::from_entries(&entries).unwrap()
}

#[tokio::test]
async fn an_endpoint_serving_another_chain_refuses_the_start_and_a_dead_one_warns() {
    let sepolia = sepolia_node().await;
    ensure_rpc_chains(&urls(&[("ethereum-sepolia", &sepolia)]), true)
        .await
        .unwrap();

    let error = ensure_rpc_chains(
        &urls(&[
            ("ethereum-sepolia", &sepolia),
            ("ethereum-mainnet", &sepolia),
        ]),
        false,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("chain ethereum-mainnet") && error.contains("observed 11155111"),
        "{error}"
    );
    assert!(!error.contains("secret-key") && !error.contains("127.0.0.1"));

    ensure_rpc_chains(
        &urls(&[("ethereum-mainnet", "http://127.0.0.1:1/secret-key")]),
        true,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn base_sepolia_is_checked_against_the_api_chain_registry() {
    let sepolia = sepolia_node().await;
    let error = ensure_rpc_chains(&urls(&[("base-sepolia", &sepolia)]), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("expected chain id 84532, observed 11155111"),
        "{error}"
    );
}
