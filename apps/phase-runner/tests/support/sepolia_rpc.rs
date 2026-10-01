use anyhow::Result;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

const SEPOLIA_GENESIS: &str = "0x25a5cc106eea7138acab33231d7160d69cb777ee0c2c553fcddf5138993e6dd9";

/// An endpoint that passes the runner's startup RPC chain check for Sepolia and answers
/// nothing else, so a runner pointed at it gets as far as its database start-up.
pub struct SepoliaIdentityRpc {
    pub endpoint: String,
    server: JoinHandle<std::io::Result<()>>,
}

impl Drop for SepoliaIdentityRpc {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl SepoliaIdentityRpc {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let router = Router::new().route("/", post(answer));
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(Self { endpoint, server })
    }
}

async fn answer(Json(call): Json<Value>) -> Json<Value> {
    let result = match call["method"].as_str() {
        Some("eth_chainId") => json!("0xaa36a7"),
        Some("eth_getBlockByNumber") if call["params"][0] == "0x0" => json!({
            "number": "0x0", "hash": SEPOLIA_GENESIS,
            "parentHash": format!("0x{}", "00".repeat(32)), "timestamp": "0x0"
        }),
        _ => Value::Null,
    };
    Json(json!({"jsonrpc": "2.0", "id": call["id"], "result": result}))
}
