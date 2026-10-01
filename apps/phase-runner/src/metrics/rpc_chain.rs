//! What the RPC chain check last saw for each configured endpoint.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use bigname_metrics::{IntGaugeVec, MetricsRegistry};

/// `(chain, source)` → (reported numeric chain id or -1, mismatch).
#[derive(Clone, Default)]
pub(super) struct RpcChainStates {
    inner: Arc<Mutex<BTreeMap<(String, String), (i64, bool)>>>,
}

impl RpcChainStates {
    pub(super) fn record(&self, chain: &str, source: &str, chain_id: Option<u64>, mismatch: bool) {
        let chain_id = chain_id.and_then(|id| i64::try_from(id).ok()).unwrap_or(-1);
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((chain.to_owned(), source.to_owned()), (chain_id, mismatch));
    }

    fn snapshot(&self) -> BTreeMap<(String, String), (i64, bool)> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[derive(Clone)]
pub(super) struct RpcChainGauges {
    chain_id: IntGaugeVec,
    mismatch: IntGaugeVec,
}

impl RpcChainGauges {
    pub(super) fn new(registry: &MetricsRegistry) -> Result<Self> {
        Ok(Self {
            chain_id: registry.int_gauge_vec(
                "phase_runner_rpc_chain_id",
                "EIP-155 chain id the RPC endpoint reported to its latest RPC chain check, or -1 \
                 when it reported none that could be read.",
                &["chain", "source"],
            )?,
            mismatch: registry.int_gauge_vec(
                "phase_runner_rpc_chain_mismatch",
                "1 when the RPC endpoint failed its RPC chain check and the chain stopped, else 0.",
                &["chain", "source"],
            )?,
        })
    }

    pub(super) fn apply(&self, states: &RpcChainStates) {
        for ((chain, source), (chain_id, mismatch)) in states.snapshot() {
            let labels = &[chain.as_str(), source.as_str()];
            self.chain_id.with_label_values(labels).set(chain_id);
            self.mismatch
                .with_label_values(labels)
                .set(i64::from(mismatch));
        }
    }
}
