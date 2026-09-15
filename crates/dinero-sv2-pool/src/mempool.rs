//! Transaction selection and Utreexo deletions for pool-owned templates.
//! After a proof failure the daemon rebuilds the excluded transaction set,
//! including fees, witnesses, filters, and height-dependent shielded state.

use crate::{
    mapper::{self, MempoolTx, PoolTemplate},
    rpc,
};
use anyhow::{anyhow, ensure, Context, Result};
use dinero_sv2_jd::{commitment, leaf_hash_for_height, DeletionTarget, UtreexoAccumulatorState};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

type Outpoint = ([u8; 32], u32);

fn display_txid(raw: &[u8; 32]) -> String {
    hex::encode(raw.iter().rev().copied().collect::<Vec<_>>())
}

fn hash(value: &Value) -> Result<[u8; 32]> {
    let bytes = hex::decode(value.as_str().context("missing hash")?)?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("hash must be 32 bytes"))
}

fn parse_proof(
    p: &Value,
    outpoint: &Outpoint,
    pre: &UtreexoAccumulatorState,
) -> Result<DeletionTarget> {
    ensure!(p["success"] == true, "proof failed: {}", p["error"]);
    ensure!(
        p["txid"].as_str() == Some(display_txid(&outpoint.0).as_str())
            && p["vout"].as_u64() == Some(u64::from(outpoint.1)),
        "proof outpoint mismatch"
    );
    ensure!(
        p["num_leaves"].as_u64() == Some(pre.num_leaves),
        "proof leaf count changed"
    );
    let position = p["position"].as_u64().context("missing proof position")?;
    ensure!(position < pre.num_leaves, "proof position out of range");
    let leaf_hash = hash(&p["leaf_hash"])?;
    let siblings = p["siblings"]
        .as_array()
        .context("missing siblings")?
        .iter()
        .map(hash)
        .collect::<Result<Vec<_>>>()?;
    let height = dinero_sv2_jd::utreexo::tree_height_for_position(pre.num_leaves, position);
    ensure!(
        siblings.len() == usize::from(height),
        "proof sibling count mismatch"
    );
    let local = position - dinero_sv2_jd::utreexo::tree_start_position(pre.num_leaves, height);
    let mut root = leaf_hash;
    for (level, sibling) in siblings.iter().enumerate() {
        root = if (local >> level) & 1 == 0 {
            dinero_sv2_jd::utreexo::node_hash(&root, sibling)
        } else {
            dinero_sv2_jd::utreexo::node_hash(sibling, &root)
        };
    }
    let index = (pre.num_leaves & ((1u64 << height) - 1)).count_ones() as usize;
    ensure!(
        pre.forest_roots.get(index) == Some(&root),
        "proof does not match template forest"
    );
    Ok(DeletionTarget {
        position,
        leaf_hash,
        siblings,
    })
}

struct Selection {
    proofs: Vec<Vec<DeletionTarget>>,
    dropped: HashSet<usize>,
}

async fn prove_transactions(
    rpc: &rpc::RpcClient,
    pre: &UtreexoAccumulatorState,
    txs: &[MempoolTx],
) -> Result<Selection> {
    pre.validate()?;
    let ids: HashMap<_, _> = txs
        .iter()
        .enumerate()
        .map(|(i, tx)| (tx.txid_raw, i))
        .collect();
    ensure!(ids.len() == txs.len(), "duplicate template transaction");
    let mut requests = Vec::new();
    let mut selection = Selection {
        proofs: vec![vec![]; txs.len()],
        dropped: HashSet::new(),
    };
    for (i, tx) in txs.iter().enumerate() {
        for outpoint in &tx.inputs {
            if let Some(&parent) = ids.get(&outpoint.0) {
                // Ephemeral outputs never enter the chain-tip forest.
                if parent >= i || outpoint.1 as usize >= txs[parent].outputs.len() {
                    selection.dropped.insert(i);
                }
            } else {
                requests.push((i, *outpoint));
            }
        }
    }
    // getproofupdates is the bounded batch endpoint with leaf_hash and flat
    // position/siblings. getutxoproofs_batch omits leaf_hash and nests proofs.
    // No requests at all for shielded-only inputs.
    for batch in requests.chunks(100) {
        let outpoints: Vec<_> = batch
            .iter()
            .map(|(_, (txid, vout))| (display_txid(txid), *vout))
            .collect();
        let response = rpc.get_utxo_proof_updates(&outpoints).await;
        let validated = response.and_then(|v| {
            ensure!(v["status"] == "updated", "missing updated proof batch");
            ensure!(
                hash(&v["root_to"])? == commitment(pre)?,
                "proof batch root changed"
            );
            let proofs = v["proofs"].as_array().context("missing proofs")?;
            ensure!(proofs.len() == batch.len(), "proof batch length mismatch");
            Ok(proofs.clone())
        });
        match validated {
            Ok(proofs) => {
                for ((index, outpoint), proof) in batch.iter().zip(proofs) {
                    match parse_proof(&proof, outpoint, pre) {
                        Ok(p) => selection.proofs[*index].push(p),
                        Err(error) => {
                            tracing::warn!(txid = %display_txid(&txs[*index].txid_raw), error = %error, "excluding transaction after proof failure");
                            selection.dropped.insert(*index);
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, count = batch.len(), "excluding transactions from failed proof batch");
                selection.dropped.extend(batch.iter().map(|(i, _)| *i));
            }
        }
    }
    Ok(selection)
}

fn drop_descendants(txs: &[MempoolTx], dropped: &mut HashSet<usize>) {
    loop {
        let ids: HashSet<_> = dropped.iter().map(|i| txs[*i].txid_raw).collect();
        let before = dropped.len();
        for (i, tx) in txs.iter().enumerate() {
            if tx.inputs.iter().any(|(id, _)| ids.contains(id)) {
                dropped.insert(i);
            }
        }
        if dropped.len() == before {
            break;
        }
    }
}

/// Apply surviving transaction outputs AFTER coinbase outputs, in block order.
/// Outputs created and spent within the block never enter the forest.
pub fn add_transaction_outputs(
    state: &mut UtreexoAccumulatorState,
    txs: &[MempoolTx],
    height: u32,
    maturity: u32,
) -> Result<()> {
    let spent: HashSet<_> = txs
        .iter()
        .flat_map(|tx| tx.inputs.iter().copied())
        .collect();
    for tx in txs {
        for (vout, (value, script)) in tx.outputs.iter().enumerate() {
            if !spent.contains(&(tx.txid_raw, vout as u32)) {
                state.add_leaf(leaf_hash_for_height(
                    &tx.txid_raw,
                    vout as u32,
                    *value,
                    script,
                    height,
                    false,
                    maturity,
                ))?;
            }
        }
    }
    Ok(())
}

/// Prepare the post-deletion forest. The daemon owns all transaction-set
/// commitments, including the height-dependent shielded anchor history. After
/// any proof failure, request a newly assembled template with exclusions.
/// Verify the daemon honored them; older daemons must fail closed.
pub async fn prepare_template(
    rpc: &rpc::RpcClient,
    mut pt: PoolTemplate,
    address: &str,
) -> Result<PoolTemplate> {
    let pre = pt
        .utreexo_pre_block
        .clone()
        .context("missing pre-block forest")?;
    let parent = pt.wire.prev_block_hash;
    let height = pt.height;
    let template_id = pt.wire.template_id;
    let needs_dnrs = mapper::state_commitment_root(&pt.coinbase_full_hex)?.is_some();
    let mut excluded = HashSet::new();
    // Whole-operation timeout is applied by the caller, separately from each
    // HTTP timeout. Bound retries too, since new mempool transactions can arrive.
    for _ in 0..4 {
        let mut selection = prove_transactions(rpc, &pre, &pt.mempool_txs).await?;
        drop_descendants(&pt.mempool_txs, &mut selection.dropped);
        if selection.dropped.is_empty() {
            let mut state = pre;
            state.apply_deletions(&selection.proofs.into_iter().flatten().collect::<Vec<_>>())?;
            pt.utreexo_pre_block = Some(state);
            return Ok(pt);
        }
        excluded.extend(
            selection
                .dropped
                .iter()
                .map(|i| display_txid(&pt.mempool_txs[*i].txid_raw)),
        );
        let mut excluded_ids: Vec<_> = excluded.iter().cloned().collect();
        excluded_ids.sort();
        let gbt = rpc
            .get_block_template_excluding(address, &excluded_ids)
            .await?;
        pt = mapper::map_template(&gbt, template_id)?;
        ensure!(
            pt.wire.prev_block_hash == parent && pt.height == height,
            "template parent changed during proof recovery"
        );
        ensure!(
            pt.mempool_txs
                .iter()
                .all(|tx| !excluded.contains(&display_txid(&tx.txid_raw))),
            "daemon did not honor template transaction exclusions; upgrade backend"
        );
        let filtered_dnrs = mapper::state_commitment_root(&pt.coinbase_full_hex)?;
        ensure!(
            !needs_dnrs || filtered_dnrs.is_some(),
            "filtered template lost DNRS"
        );
        tracing::warn!(
            excluded = excluded.len(),
            retained = pt.mempool_txs.len(),
            "daemon rebuilt template after proof failures"
        );
    }
    anyhow::bail!("transaction proof recovery retry limit reached")
}

#[cfg(test)]
async fn apply_mempool_to_pre_coinbase(
    rpc: &rpc::RpcClient,
    pre: &UtreexoAccumulatorState,
    txs: &[MempoolTx],
    height: u32,
    maturity: u32,
) -> Result<UtreexoAccumulatorState> {
    let selection = prove_transactions(rpc, pre, txs).await?;
    ensure!(selection.dropped.is_empty(), "proof failure");
    let mut state = pre.clone();
    state.apply_deletions(&selection.proofs.into_iter().flatten().collect::<Vec<_>>())?;
    add_transaction_outputs(&mut state, txs, height, maturity)?;
    Ok(state)
}
#[cfg(test)]
mod tests {
    use super::*;
    use dinero_sv2_jd::{commitment, UtreexoAccumulatorState};
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn proof_verification_rejects_bad_identity_shape_and_stale_forest() {
        let mut pre = UtreexoAccumulatorState::empty();
        pre.add_leaf([9; 32]).unwrap();
        let good = json!({"success":true,"txid":display_txid(&[8;32]),"vout":2,
            "position":0,"num_leaves":1,"leaf_hash":hex::encode([9;32]),"siblings":[]});
        assert!(parse_proof(&good, &([8; 32], 2), &pre).is_ok());
        for (field, value) in [
            ("success", json!(false)),
            ("txid", json!(hex::encode([1; 32]))),
            ("vout", json!(3)),
            ("position", json!(1)),
            ("num_leaves", json!(2)),
            ("leaf_hash", json!(hex::encode([1; 32]))),
            ("siblings", json!([hex::encode([1; 32])])),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(
                parse_proof(&bad, &([8; 32], 2), &pre).is_err(),
                "accepted invalid {field}"
            );
        }
    }

    #[tokio::test]
    async fn intra_block_spends_need_no_rpc_and_never_add_ephemeral_outputs() {
        let rpc = rpc::RpcClient::new(
            "http://127.0.0.1:1".into(),
            rpc::Auth::UserPass("test".into(), "test".into()),
        )
        .unwrap();
        let parent = unshield();
        let child = mapper::MempoolTx {
            txid_raw: [8; 32],
            inputs: vec![([7; 32], 0)],
            ..unshield()
        };
        let post = apply_mempool_to_pre_coinbase(
            &rpc,
            &UtreexoAccumulatorState::empty(),
            &[parent, child],
            100,
            20,
        )
        .await
        .unwrap();
        assert_eq!(post.num_leaves, 1);
        assert_eq!(
            post.forest_roots,
            vec![leaf_hash_for_height(
                &[8; 32],
                0,
                123,
                &[0x51],
                100,
                false,
                20
            )]
        );
    }

    #[tokio::test]
    async fn proof_rpc_rejects_empty_and_oversize_batches_without_network() {
        let rpc = rpc::RpcClient::new(
            "http://127.0.0.1:1".into(),
            rpc::Auth::UserPass("test".into(), "test".into()),
        )
        .unwrap();
        for outpoints in [vec![], vec![(hex::encode([1; 32]), 0); 101]] {
            assert!(rpc
                .get_utxo_proof_updates(&outpoints)
                .await
                .unwrap_err()
                .to_string()
                .contains("1-100"));
        }
    }

    #[tokio::test]
    async fn proof_requests_are_chunked_at_daemon_limit() {
        let pre = UtreexoAccumulatorState::empty();
        let root = hex::encode(commitment(&pre).unwrap());
        let (rpc, server) = fake_rpc(vec![
            json!({"status":"updated","root_to":root,"proofs":vec![json!({"success":false});100]}),
            json!({"status":"updated","root_to":root,"proofs":[{"success":false}]}),
        ])
        .await;
        let tx = mapper::MempoolTx {
            inputs: (0..101).map(|i| ([8; 32], i)).collect(),
            ..unshield()
        };
        let selection = prove_transactions(&rpc, &pre, &[tx]).await.unwrap();
        assert_eq!(selection.dropped, HashSet::from([0]));
        let requests = server.await.unwrap();
        assert_eq!(
            requests[0]["params"][0]["outpoints"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
        assert_eq!(
            requests[1]["params"][0]["outpoints"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn old_daemon_ignoring_exclusions_cannot_publish_invalid_recovery_work() {
        let mut pt = mapper::tests::fixture_pool_template();
        pt.mempool_txs = vec![mapper::MempoolTx {
            inputs: vec![([9; 32], 0)],
            ..unshield()
        }];
        let mut unchanged = mapper::tests::fixture();
        unchanged["transactions"] = json!([{"txid":hex::encode([7;32]),
            "data":"06000000000100017b0000000000000001510000000000"}]);
        let (rpc, server) = fake_rpc(vec![
            json!({"status":"updated",
            "root_to":hex::encode(commitment(pt.utreexo_pre_block.as_ref().unwrap()).unwrap()),
            "proofs":[{"success":false,"error":"missing UTXO"}]}),
            unchanged,
        ])
        .await;
        let error = prepare_template(&rpc, pt, "test-address")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not honor"));
        server.await.unwrap();
    }

    async fn fake_rpc(
        results: Vec<Value>,
    ) -> (rpc::RpcClient, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for result in results {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let request = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let len: usize = String::from_utf8_lossy(&bytes[..end])
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + len {
                            break serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + len])
                                .unwrap();
                        }
                    }
                };
                let body = json!({"result": result, "error": null}).to_string();
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                requests.push(request);
            }
            requests
        });
        (
            rpc::RpcClient::new(url, rpc::Auth::UserPass("test".into(), "test".into())).unwrap(),
            server,
        )
    }

    fn unshield() -> mapper::MempoolTx {
        mapper::MempoolTx {
            data: vec![6, 0, 0, 0, 0, 1, 0],
            txid_raw: [7; 32],
            inputs: vec![],
            outputs: vec![(123, vec![0x51])],
        }
    }

    #[tokio::test]
    async fn failed_proof_drops_transaction_and_descendants_but_keeps_unshield() {
        let mut pt = mapper::tests::fixture_pool_template();
        let bad = mapper::MempoolTx {
            data: vec![2, 0, 0, 0],
            txid_raw: [8; 32],
            inputs: vec![([9; 32], 0)],
            outputs: vec![(45, vec![0x52])],
        };
        let child = mapper::MempoolTx {
            data: vec![2, 0, 0, 0],
            txid_raw: [10; 32],
            inputs: vec![([8; 32], 0)],
            outputs: vec![(40, vec![0x53])],
        };
        pt.mempool_txs = vec![bad, child, unshield()];
        let original_value = pt.coinbase_value_una;
        let mut filtered = mapper::tests::fixture();
        filtered["coinbasevalue"] = json!(original_value - 15);
        filtered["transactions"] = json!([{"txid":hex::encode([7;32]),
            "data":"06000000000100017b0000000000000001510000000000", "fee":2}]);
        let (rpc, server) = fake_rpc(vec![json!({"status":"updated", "root_to":hex::encode(commitment(pt.utreexo_pre_block.as_ref().unwrap()).unwrap()), "proofs":[{"success":false,"error":"missing UTXO"}]}), filtered.clone()]).await;
        let built = prepare_template(&rpc, pt, "test-address")
            .await
            .expect("one bad tx must not stop template production");
        assert_eq!(built.mempool_txs.len(), 1);
        assert_eq!(built.mempool_txs[0].txid_raw, [7; 32]);
        assert_eq!(built.coinbase_value_una, original_value - 15);
        assert_eq!(built.merkle_path, vec![[7; 32]]);
        let requests = server.await.unwrap();
        assert_eq!(requests[1]["method"], "getblocktemplate");
        assert_eq!(
            requests[1]["params"],
            json!([{"address":"test-address", "exclude_txids":[hex::encode([8;32]),hex::encode([10;32])]}])
        );
        assert_eq!(
            built.coinbase_full_hex,
            filtered["coinbasetxn"]["data"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn zero_transparent_inputs_never_call_proof_rpc_but_add_outputs() {
        let rpc = rpc::RpcClient::new(
            "http://127.0.0.1:1".into(),
            rpc::Auth::UserPass("test".into(), "test".into()),
        )
        .unwrap();
        let state = apply_mempool_to_pre_coinbase(
            &rpc,
            &UtreexoAccumulatorState::empty(),
            &[unshield()],
            100,
            20,
        )
        .await
        .unwrap();
        assert_eq!(state.num_leaves, 1);
        assert_eq!(
            state.forest_roots,
            vec![leaf_hash_for_height(
                &[7; 32],
                0,
                123,
                &[0x51],
                100,
                false,
                20
            )]
        );
    }

    #[tokio::test]
    async fn mixed_unshield_and_transparent_use_complete_daemon_proof_contract() {
        let leaf = [9; 32];
        let mut pre = UtreexoAccumulatorState::empty();
        pre.add_leaf(leaf).unwrap();
        let (rpc, server) = fake_rpc(vec![json!({"status":"updated", "root_to":hex::encode(commitment(&pre).unwrap()), "proofs":[{
            "success":true, "txid":hex::encode([8;32]), "vout":2, "leaf_hash":hex::encode(leaf), "position":0, "num_leaves":1, "siblings":[]
        }]})]).await;
        let transparent = mapper::MempoolTx {
            txid_raw: [10; 32],
            inputs: vec![([8; 32], 2)],
            outputs: vec![],
            ..unshield()
        };
        let post = apply_mempool_to_pre_coinbase(&rpc, &pre, &[unshield(), transparent], 100, 20)
            .await
            .unwrap();
        assert_eq!(post.num_leaves, 2); // deletions leave tombstones
        let req = server.await.unwrap().remove(0);
        assert_eq!(req["method"], "getproofupdates");
        assert_eq!(
            req["params"],
            json!([{"outpoints":[{"txid":hex::encode([8;32]),"vout":2}]}])
        );
    }
}
