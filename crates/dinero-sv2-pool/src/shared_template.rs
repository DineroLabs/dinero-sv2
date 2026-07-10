//! Pool-owned template for shared-mode miners: the pool assembles the
//! entire coinbase (PPLNS split + DNRW + DNRF) and computes the header
//! roots itself. Shared miners grind the header verbatim.

use anyhow::{anyhow, Context, Result};
use dinero_sv2_common::NewTemplateDinero;
use dinero_sv2_jd::{
    assemble_stripped_coinbase,
    block_filter::{gcs_build, gcs_filter_hash},
    commitment, compute_root,
    filter_commitment::{build_dnrf_script, requires_filter_commitment},
    leaf_hash_for_height,
    witness_commitment::{build_dnrw_script_coinbase_only, requires_witness_commitment},
    CoinbaseOutput, UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
};

use crate::block::wrap_stripped_with_segwit_witness;
use crate::mapper::{MempoolTx, PoolTemplate};

/// Pool-assembled template for a shared (non-JD) miner: the pool
/// builds the whole coinbase itself, so the header roots it emits are
/// final — the miner grinds nonce/timestamp/version only.
#[derive(Debug)]
pub struct SharedTemplate {
    /// Miner-facing wire message: `merkle_root` + `utreexo_root` are
    /// the pool's own recomputation over the split coinbase, not the
    /// daemon's original template values.
    pub wire: NewTemplateDinero,
    /// Segwit-wrapped full coinbase serialization (hex), ready to
    /// splice into a block for `submitblock`.
    pub coinbase_full_hex: String,
    /// Full output list as assembled: the caller's value outputs
    /// (contributors + fee), then DNRW (if mandatory at this height),
    /// then DNRF (if mandatory at this height). Kept for logs/audit
    /// and so callers can independently recompute the Utreexo root.
    pub outputs: Vec<CoinbaseOutput>,
}

/// Assemble a pool-owned coinbase from a PPLNS `split_outputs` list
/// (value outputs only — no DNRW/DNRF), append the mandatory witness
/// and filter commitments, and recompute both header roots against
/// `pt`'s pre-block Utreexo state and merkle path.
///
/// `split_outputs` must sum exactly to `pt.coinbase_value_una`;
/// `compute_split` no longer guarantees a fee output or forbids
/// zero-value entries from being absent, so the only invariant this
/// function enforces on the caller's list is the value sum.
pub fn build_shared_template(
    pt: &PoolTemplate,
    split_outputs: Vec<CoinbaseOutput>,
) -> Result<SharedTemplate> {
    let value_sum: u64 = split_outputs.iter().map(|o| o.value_una).sum();
    if value_sum != pt.coinbase_value_una {
        return Err(anyhow!(
            "split sum {value_sum} != coinbase value {}",
            pt.coinbase_value_una
        ));
    }
    if !pt.mempool_txs.is_empty() {
        // Shared jobs are coinbase-only for now (spec: out of scope).
        return Err(anyhow!("shared templates are coinbase-only"));
    }
    let pre_block = pt
        .utreexo_pre_block
        .as_ref()
        .ok_or_else(|| anyhow!("template lacks utreexo pre-block state"))?;

    let mut outputs = split_outputs;

    // DNRW (coinbase-only constant): witness merkle root is fixed at
    // sha256d(64 zero bytes) because a coinbase-only block's witness
    // leaf is the BIP-141 zero convention and there are no mempool
    // wtxids to fold in.
    if requires_witness_commitment(pt.height as u64) {
        outputs.push(CoinbaseOutput {
            value_una: 0,
            script_pubkey: build_dnrw_script_coinbase_only(),
        });
    }
    if requires_filter_commitment(pt.height as u64) {
        // Daemon filter-input rule (verified 2026-07-09 against the
        // dinero-v8 daemon source, not just the brief's sketch):
        // ConnectTip's accept-time filter rebuild collects every
        // non-empty output scriptPubKey across the block EXCEPT ones
        // starting with OP_RETURN (0x6a) — see
        // src/daemon/services/chainstate_service.cpp:12962-12969
        // (`if (!out.scriptPubKey.empty() && out.scriptPubKey[0] !=
        // 0x6a)`), and the block assembler mirrors the identical rule
        // at src/mining/block_assembler.cpp:556-563. The gate is the
        // OP_RETURN prefix, NOT output value — a hypothetical
        // zero-value non-OP_RETURN output would still enter the
        // filter. In this builder's construction the only zero-value
        // outputs are DNRW/DNRF themselves (both OP_RETURN), so
        // filtering by script prefix and filtering by `value_una > 0`
        // happen to coincide here, but the script-prefix rule is what
        // we mirror since it's what the daemon actually enforces.
        // Existing single-payout JD miners already build their GCS
        // filter over just `[payout_script]` — see
        // crates/dinero-sv2-miner/src/main.rs:531
        // (`gcs_build(&tmpl.prev_block_hash, &[&payout_script])`) —
        // which implicitly excludes their own OP_RETURN commitments
        // and is exactly the daemon rule confirmed above.
        let script_refs: Vec<&[u8]> = outputs
            .iter()
            .filter(|o| o.script_pubkey.first() != Some(&0x6a))
            .map(|o| o.script_pubkey.as_slice())
            .collect();
        let (encoded_filter, _) = gcs_build(&pt.wire.prev_block_hash, &script_refs);
        let dnrf = build_dnrf_script(&gcs_filter_hash(&encoded_filter));
        outputs.push(CoinbaseOutput {
            value_una: 0,
            script_pubkey: dnrf,
        });
    }

    let (coinbase_stripped, coinbase_txid) =
        assemble_stripped_coinbase(&pt.coinbase_prefix, &outputs, &pt.coinbase_suffix);

    let mut state = pre_block.clone();
    for (i, o) in outputs.iter().enumerate() {
        state
            .add_leaf(leaf_hash_for_height(
                &coinbase_txid,
                i as u32,
                o.value_una,
                &o.script_pubkey,
                pt.height,
                true,
                UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
            ))
            .context("shared template add_leaf")?;
    }
    let utreexo_root = commitment(&state).context("shared template commitment")?;
    let merkle_root = compute_root(coinbase_txid, &pt.merkle_path);

    // Re-wrap the stripped coinbase with the daemon's witness bytes for
    // submitblock (same helper the extended-share path uses).
    let full_coinbase = wrap_stripped_with_segwit_witness(
        &coinbase_stripped,
        &pt.coinbase_witness_bytes,
        &pt.coinbase_suffix,
    );

    let wire = NewTemplateDinero {
        merkle_root,
        utreexo_root,
        ..pt.wire.clone()
    };

    Ok(SharedTemplate {
        wire,
        coinbase_full_hex: hex::encode(full_coinbase),
        outputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_template_coinbase_and_roots_are_consistent() {
        let mut pt = crate::mapper::tests::fixture_pool_template();
        // Bump past both DNRW's and DNRF's mandatory-commitment
        // heights so this test exercises the full output list (value
        // outputs + DNRW + DNRF), matching the `outputs` doc comment.
        pt.height = 61_410;

        let outputs = vec![
            CoinbaseOutput {
                value_una: pt.coinbase_value_una - 200_000_000,
                script_pubkey: vec![0x51, 0x20, 0x01],
            },
            CoinbaseOutput {
                value_una: 200_000_000,
                script_pubkey: vec![0x51, 0x20, 0x09],
            },
        ];
        let st = build_shared_template(&pt, outputs.clone()).unwrap();

        // Value invariant:
        let cb = hex::decode(&st.coinbase_full_hex).unwrap();
        assert!(cb.len() > 100);
        // Full output list = 2 value outputs + DNRW + DNRF.
        assert_eq!(st.outputs.len(), 4);
        assert_eq!(st.outputs[0], outputs[0]);
        assert_eq!(st.outputs[1], outputs[1]);
        assert!(st.outputs[2].script_pubkey.starts_with(&[0x6a, 0x25, 0x44, 0x4E, 0x52, 0x57])); // DNRW
        assert!(st.outputs[3].script_pubkey.starts_with(&[0x6a, 0x25, 0x44, 0x4E, 0x52, 0x46])); // DNRF

        // Roots differ from the daemon-template ones (different coinbase):
        assert_ne!(st.wire.merkle_root, pt.wire.merkle_root);
        assert_ne!(st.wire.utreexo_root, pt.wire.utreexo_root);

        // Recompute root independently with v2 leaves and compare:
        let full_outputs = st.outputs.clone(); // value outputs + DNRW + DNRF
        let (_, txid) =
            assemble_stripped_coinbase(&pt.coinbase_prefix, &full_outputs, &pt.coinbase_suffix);
        let mut state = pt.utreexo_pre_block.clone().unwrap();
        for (i, o) in full_outputs.iter().enumerate() {
            state
                .add_leaf(leaf_hash_for_height(
                    &txid,
                    i as u32,
                    o.value_una,
                    &o.script_pubkey,
                    pt.height,
                    true,
                    UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
                ))
                .unwrap();
        }
        assert_eq!(st.wire.utreexo_root, commitment(&state).unwrap());
    }

    #[test]
    fn rejects_split_sum_mismatching_coinbase_value() {
        let pt = crate::mapper::tests::fixture_pool_template();
        let outputs = vec![CoinbaseOutput {
            value_una: pt.coinbase_value_una - 1,
            script_pubkey: vec![0x51, 0x20, 0x01],
        }];
        let err = build_shared_template(&pt, outputs).unwrap_err();
        assert!(err.to_string().contains("split sum"));
    }

    #[test]
    fn rejects_missing_utreexo_pre_block() {
        let mut pt = crate::mapper::tests::fixture_pool_template();
        pt.utreexo_pre_block = None;
        let outputs = vec![CoinbaseOutput {
            value_una: pt.coinbase_value_una,
            script_pubkey: vec![0x51, 0x20, 0x01],
        }];
        let err = build_shared_template(&pt, outputs).unwrap_err();
        assert!(err.to_string().contains("utreexo pre-block"));
    }

    #[test]
    fn rejects_nonempty_mempool() {
        let mut pt = crate::mapper::tests::fixture_pool_template();
        // Fixture has empty mempool_txs; push a dummy one to trigger rejection.
        pt.mempool_txs.push(MempoolTx {
            data: vec![0x01, 0x02, 0x03], // minimal dummy tx bytes
            txid_raw: [0x42u8; 32],
            inputs: vec![],
            outputs: vec![],
        });

        let outputs = vec![CoinbaseOutput {
            value_una: pt.coinbase_value_una,
            script_pubkey: vec![0x51, 0x20, 0x01],
        }];
        let err = build_shared_template(&pt, outputs).unwrap_err();
        assert!(err.to_string().contains("coinbase-only"));
    }

    #[test]
    fn below_activation_heights_skip_commitments() {
        let mut pt = crate::mapper::tests::fixture_pool_template();
        // Height 2: below DNRW mandatory (10_670) but at/above DNRF activation (1).
        pt.height = 2;
        assert!(requires_filter_commitment(pt.height as u64)); // DNRF should be present
        assert!(!requires_witness_commitment(pt.height as u64)); // DNRW should be absent

        let outputs = vec![
            CoinbaseOutput {
                value_una: pt.coinbase_value_una - 200_000_000,
                script_pubkey: vec![0x51, 0x20, 0x01],
            },
            CoinbaseOutput {
                value_una: 200_000_000,
                script_pubkey: vec![0x51, 0x20, 0x09],
            },
        ];
        let st = build_shared_template(&pt, outputs.clone()).unwrap();

        // Should have 2 value outputs + DNRF only (no DNRW).
        assert_eq!(st.outputs.len(), 3);
        assert_eq!(st.outputs[0], outputs[0]);
        assert_eq!(st.outputs[1], outputs[1]);
        // DNRF: starts with 0x6a 0x25 0x44 0x4e 0x52 0x46 ("DNRF" magic).
        assert!(st.outputs[2].script_pubkey.starts_with(&[0x6a, 0x25, 0x44, 0x4E, 0x52, 0x46]));
        // Verify no script starts with DNRW prefix (0x6a 0x25 0x44 0x4e 0x52 0x57).
        assert!(!st.outputs.iter()
            .any(|o| o.script_pubkey.starts_with(&[0x6a, 0x25, 0x44, 0x4E, 0x52, 0x57])));
    }
}
