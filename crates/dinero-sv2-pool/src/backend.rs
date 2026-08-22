//! Health-aware dinerod backend selection and block-submission fanout.
//!
//! There is deliberately no quorum: Dinero follows the valid chain with the
//! greatest cumulative proof of work. A backend must also successfully serve a
//! gated `getblocktemplate`, which rejects IBD, safe-mode and active/header-tip
//! mismatch states before the pool hands its work to miners.

use crate::rpc::{RpcClient, SubmitBlockResult};
use anyhow::{anyhow, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendHealth {
    pub index: usize,
    pub endpoint: String,
    pub best_hash: String,
    pub blocks: u64,
    pub headers: u64,
    pub chainwork: String,
    pub initial_block_download: bool,
}

impl BackendHealth {
    fn from_rpc(index: usize, endpoint: String, value: &Value) -> Result<Self> {
        let chainwork = normalize_chainwork(
            value
                .get("chainwork")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .ok_or_else(|| anyhow!("backend {endpoint} returned invalid chainwork"))?;
        let best_hash = value
            .get("bestblockhash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if best_hash.is_empty() {
            return Err(anyhow!("backend {endpoint} returned no bestblockhash"));
        }
        Ok(Self {
            index,
            endpoint,
            best_hash,
            blocks: value.get("blocks").and_then(Value::as_u64).unwrap_or(0),
            headers: value.get("headers").and_then(Value::as_u64).unwrap_or(0),
            chainwork,
            initial_block_download: value
                .get("initialblockdownload")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    }
}

#[derive(Clone)]
pub struct BackendLease {
    pub client: Arc<RpcClient>,
    pub health: BackendHealth,
    pub epoch: u64,
    pub switched: bool,
}

struct SelectionState {
    active_index: Option<usize>,
    epoch: u64,
}

pub struct BackendPool {
    clients: Vec<Arc<RpcClient>>,
    selection: Mutex<SelectionState>,
}

impl BackendPool {
    pub fn new(clients: Vec<Arc<RpcClient>>) -> Result<Self> {
        if clients.is_empty() {
            return Err(anyhow!("at least one dinerod RPC backend is required"));
        }
        Ok(Self {
            clients,
            selection: Mutex::new(SelectionState {
                active_index: None,
                epoch: 0,
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Probe every backend concurrently, require a mining-safe template, then
    /// select the greatest-work result. On equal work, retain the current
    /// backend to prevent oscillation; otherwise use stable CLI order.
    pub async fn select_template(&self, payout: &str) -> Result<(BackendLease, Value)> {
        let mut probes = JoinSet::new();
        for (index, client) in self.clients.iter().cloned().enumerate() {
            let payout = payout.to_owned();
            probes.spawn(async move {
                let info = client.get_blockchain_info().await?;
                let health = BackendHealth::from_rpc(index, client.endpoint().to_owned(), &info)?;
                if health.initial_block_download {
                    return Err(anyhow!("{} is in initial block download", health.endpoint));
                }
                // This RPC is the authoritative daemon safety gate. In
                // particular it rejects equal-height active/header hash splits.
                let template = client.get_block_template(&payout).await?;
                Ok::<_, anyhow::Error>((client, health, template))
            });
        }

        let mut ready = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = probes.join_next().await {
            match result {
                Ok(Ok(candidate)) => ready.push(candidate),
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(format!("backend probe task failed: {e}")),
            }
        }
        if ready.is_empty() {
            return Err(anyhow!(
                "no mining-safe dinerod backend: {}",
                errors.join("; ")
            ));
        }

        let current = self
            .selection
            .lock()
            .expect("backend selection mutex")
            .active_index;
        let selected = choose_best_candidate(&ready, current);
        let (client, health, template) = ready.swap_remove(selected);

        let (epoch, switched) = {
            let mut state = self.selection.lock().expect("backend selection mutex");
            let switched = state.active_index != Some(health.index);
            if switched {
                state.epoch = state.epoch.wrapping_add(1).max(1);
                state.active_index = Some(health.index);
            }
            (state.epoch, switched)
        };

        Ok((
            BackendLease {
                client,
                health,
                epoch,
                switched,
            },
            template,
        ))
    }

    /// Submit to every configured backend concurrently. Any acceptance wins.
    /// A transport timeout is reconciled by querying the submitted block hash,
    /// eliminating the old "acceptance unknown" operational state.
    pub async fn submit_block(&self, block_hex: &str) -> Result<SubmitBlockResult> {
        let hash_candidates = block_hash_candidates(block_hex)?;
        let mut submits = JoinSet::new();
        for client in &self.clients {
            let client = Arc::clone(client);
            let block_hex = block_hex.to_owned();
            let hashes = hash_candidates.clone();
            submits.spawn(async move {
                match client.submit_block(&block_hex).await {
                    ok @ Ok(SubmitBlockResult::Accepted) => ok,
                    Ok(SubmitBlockResult::Rejected(reason)) => {
                        Ok(SubmitBlockResult::Rejected(reason))
                    }
                    Err(submit_error) => {
                        for hash in &hashes {
                            if client.has_block(hash).await {
                                return Ok(SubmitBlockResult::Accepted);
                            }
                        }
                        Err(submit_error)
                    }
                }
            });
        }

        let mut accepted = false;
        let mut rejections = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = submits.join_next().await {
            match result {
                Ok(Ok(SubmitBlockResult::Accepted)) => accepted = true,
                Ok(Ok(SubmitBlockResult::Rejected(reason))) => rejections.push(reason),
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(format!("submit task failed: {e}")),
            }
        }
        if accepted {
            return Ok(SubmitBlockResult::Accepted);
        }
        if !rejections.is_empty() {
            return Ok(SubmitBlockResult::Rejected(rejections.join("; ")));
        }
        Err(anyhow!(
            "all block submissions failed: {}",
            errors.join("; ")
        ))
    }
}

fn normalize_chainwork(input: &str) -> Option<String> {
    if input.is_empty() || !input.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let normalized = input.trim_start_matches('0').to_ascii_lowercase();
    Some(if normalized.is_empty() {
        "0".into()
    } else {
        normalized
    })
}

fn compare_chainwork(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn choose_best_candidate(
    candidates: &[(Arc<RpcClient>, BackendHealth, Value)],
    current: Option<usize>,
) -> usize {
    let mut best = 0;
    for i in 1..candidates.len() {
        let order = compare_chainwork(&candidates[i].1.chainwork, &candidates[best].1.chainwork);
        let candidate_is_current = Some(candidates[i].1.index) == current;
        let best_is_current = Some(candidates[best].1.index) == current;
        let stable_tie_winner = candidate_is_current
            || (!best_is_current && candidates[i].1.index < candidates[best].1.index);
        if order == Ordering::Greater || (order == Ordering::Equal && stable_tie_winner) {
            best = i;
        }
    }
    best
}

fn block_hash_candidates(block_hex: &str) -> Result<Vec<String>> {
    let bytes = hex::decode(block_hex)?;
    if bytes.len() < 128 {
        return Err(anyhow!(
            "serialized block is shorter than the 128-byte header"
        ));
    }
    let first = Sha256::digest(&bytes[..128]);
    let second = Sha256::digest(first);
    let raw = hex::encode(second);
    let display = second.iter().rev().map(|b| format!("{b:02x}")).collect();
    Ok(if raw == display {
        vec![raw]
    } else {
        vec![display, raw]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Auth;

    fn candidate(index: usize, work: &str) -> (Arc<RpcClient>, BackendHealth, Value) {
        let client = Arc::new(
            RpcClient::new(
                format!("http://127.0.0.1:{}", 21000 + index),
                Auth::UserPass("u".into(), "p".into()),
            )
            .unwrap(),
        );
        (
            client,
            BackendHealth {
                index,
                endpoint: format!("backend-{index}"),
                best_hash: format!("hash-{index}"),
                blocks: 100,
                headers: 100,
                chainwork: normalize_chainwork(work).unwrap(),
                initial_block_download: false,
            },
            Value::Null,
        )
    }

    #[test]
    fn greatest_chainwork_wins_without_voting() {
        let candidates = vec![candidate(0, "ff"), candidate(1, "0100"), candidate(2, "fe")];
        assert_eq!(choose_best_candidate(&candidates, Some(0)), 1);
    }

    #[test]
    fn equal_work_retains_current_backend() {
        let candidates = vec![candidate(2, "0100"), candidate(1, "100")];
        assert_eq!(choose_best_candidate(&candidates, Some(1)), 1);
        assert_eq!(choose_best_candidate(&candidates, Some(2)), 0);
        assert_eq!(choose_best_candidate(&candidates, None), 1);
    }

    #[test]
    fn chainwork_parser_rejects_malformed_values() {
        assert_eq!(normalize_chainwork("0000AB"), Some("ab".into()));
        assert_eq!(normalize_chainwork("000"), Some("0".into()));
        assert_eq!(normalize_chainwork("xyz"), None);
    }

    #[test]
    fn block_hash_requires_a_complete_header() {
        assert!(block_hash_candidates("00").is_err());
        assert_eq!(block_hash_candidates(&"00".repeat(128)).unwrap().len(), 2);
    }
}
