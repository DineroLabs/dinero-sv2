//! DNRW witness commitment script builder.
//!
//! Mirrors `BuildWitnessCommitment` / `FindWitnessCommitmentIndex` /
//! `ValidateWitnessCommitment` in dinerod's
//! `src/consensus/witness_commitment.cpp`. Every block with witness
//! data at or above [`WITNESS_COMMITMENT_MANDATORY_HEIGHT`] must carry
//! an OP_RETURN output in its coinbase whose script is exactly 39
//! bytes:
//!
//! ```text
//! [0]    = 0x6a             (OP_RETURN)
//! [1]    = 0x25             (push 37 bytes)
//! [2..6] = 0x44 0x4E 0x52 0x57   ("DNRW" magic)
//! [6]    = 0x01             (version)
//! [7..39]= sha256d(witness_merkle_root || 32 zero bytes)
//! ```
//!
//! The witness merkle root is BIP-141 style: leaf 0 is the coinbase
//! whose wtxid is 32 zero bytes **by convention**, followed by every
//! mempool tx's wtxid (`sha256d` of the witness-included
//! serialization); pairs hash with plain `sha256d(left || right)`,
//! duplicating the last node on odd layers. Because the coinbase leaf
//! is constant, the root — and therefore the whole DNRW script — is
//! independent of the miner's coinbase. A coinbase-only block commits
//! to the constant `sha256d(64 zero bytes)`.
//!
//! JD miners that customize the coinbase MUST include this output
//! (alongside the DNRF filter commitment) or dinerod rejects the
//! found block at ConnectTip with `missing-witness-commitment` — the
//! failure that burned pool blocks on 2026-07-09 once the utreexo
//! maturity-leaf fix let submissions get past the root check.

use sha2::{Digest, Sha256};

/// Height at which the DNRW commitment becomes mandatory for blocks
/// carrying witness data. Matches `WITNESS_COMMITMENT_MANDATORY_HEIGHT`
/// in dinerod's `src/consensus/block_validation.cpp` (blocks 1-10669
/// predate the assembler adding commitments).
pub const WITNESS_COMMITMENT_MANDATORY_HEIGHT: u64 = 10_670;

/// The 4-byte magic `"DNRW"`.
pub const DNRW_MAGIC: [u8; 4] = [0x44, 0x4E, 0x52, 0x57];

/// Commitment version (currently `0x01`).
pub const DNRW_VERSION: u8 = 0x01;

/// Script body length after the push byte (magic + version + hash).
pub const DNRW_DATA_SIZE: u8 = 37;

/// Returns `true` if a block at `height` must carry a DNRW commitment
/// in its coinbase. (Strictly dinerod only enforces it when the block
/// has witness data, but pool-submitted blocks always do — the
/// coinbase carries the segwit reserved witness.)
pub fn requires_witness_commitment(height: u64) -> bool {
    height >= WITNESS_COMMITMENT_MANDATORY_HEIGHT
}

fn sha256d(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

/// Raw (internal byte order) wtxid of a transaction: `sha256d` of its
/// full witness-included serialization — exactly the bytes the daemon
/// returns in `getblocktemplate`'s `transactions[].data`.
pub fn wtxid_from_tx_bytes(tx_bytes: &[u8]) -> [u8; 32] {
    sha256d(tx_bytes)
}

/// BIP-141-style witness merkle root over `[coinbase (zeros)] ++
/// wtxids`. `wtxids` are the raw (internal byte order) wtxids of the
/// non-coinbase transactions in block order; pass `&[]` for a
/// coinbase-only block. Matches `ComputeWitnessMerkleRoot` in
/// dinerod's `src/consensus/merkle_root.cpp`.
pub fn witness_merkle_root(wtxids: &[[u8; 32]]) -> [u8; 32] {
    let mut layer: Vec<[u8; 32]> = Vec::with_capacity(1 + wtxids.len());
    layer.push([0u8; 32]); // coinbase wtxid is zeros by convention
    layer.extend_from_slice(wtxids);

    while layer.len() > 1 {
        let mut next = Vec::with_capacity(layer.len().div_ceil(2));
        for pair in layer.chunks(2) {
            let left = &pair[0];
            let right = pair.get(1).unwrap_or(&pair[0]);
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(left);
            buf[32..].copy_from_slice(right);
            next.push(sha256d(&buf));
        }
        layer = next;
    }
    layer[0]
}

/// Build the 39-byte DNRW OP_RETURN scriptPubKey for a block whose
/// witness merkle root is `root`. The witness nonce is the BIP-141
/// default (32 zero bytes) — dinerod validates against that constant.
pub fn build_dnrw_script(root: &[u8; 32]) -> Vec<u8> {
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(root);
    // [32..64] stays zero: DEFAULT_NONCE
    let commitment = sha256d(&preimage);

    let mut s = Vec::with_capacity(2 + DNRW_DATA_SIZE as usize);
    s.push(0x6a);
    s.push(DNRW_DATA_SIZE);
    s.extend_from_slice(&DNRW_MAGIC);
    s.push(DNRW_VERSION);
    s.extend_from_slice(&commitment);
    s
}

/// Convenience: the DNRW script for a coinbase-only block (witness
/// merkle root = zeros, commitment = `sha256d(64 zero bytes)`).
pub fn build_dnrw_script_coinbase_only() -> Vec<u8> {
    build_dnrw_script(&witness_merkle_root(&[]))
}

/// Recognise a DNRW commitment script by its fixed shape. Used by the
/// pool to validate miner-supplied coinbase outputs before accepting
/// an extended share — a block missing it burns at ConnectTip.
pub fn is_dnrw_script(script: &[u8]) -> bool {
    script.len() == 2 + DNRW_DATA_SIZE as usize
        && script[0] == 0x6a
        && script[1] == DNRW_DATA_SIZE
        && script[2..6] == DNRW_MAGIC
        && script[6] == DNRW_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live-mainnet consensus vector: the daemon's GBT coinbase at
    /// height 61403 (v8.0.13) carried exactly this DNRW script for a
    /// coinbase-only template.
    #[test]
    fn coinbase_only_dnrw_matches_live_mainnet_template() {
        let expected = hex::decode(
            "6a25444e525701e2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf9",
        )
        .unwrap();
        assert_eq!(build_dnrw_script_coinbase_only(), expected);
    }

    #[test]
    fn coinbase_only_commitment_is_sha256d_of_64_zero_bytes() {
        assert_eq!(witness_merkle_root(&[]), [0u8; 32]);
        let script = build_dnrw_script_coinbase_only();
        assert_eq!(&script[7..], &sha256d(&[0u8; 64])[..]);
    }

    /// With one mempool tx the root is sha256d(zeros || wtxid) —
    /// odd-layer duplication does not apply to a 2-leaf tree.
    #[test]
    fn witness_root_single_mempool_tx() {
        let w = [0x42u8; 32];
        let mut buf = [0u8; 64];
        buf[32..].copy_from_slice(&w);
        assert_eq!(witness_merkle_root(&[w]), sha256d(&buf));
    }

    /// Three leaves (coinbase + 2 wtxids) exercises last-node
    /// duplication on the odd layer, mirroring ComputeMerkleRoot.
    #[test]
    fn witness_root_duplicates_last_on_odd_layer() {
        let a = [0x01u8; 32];
        let b = [0x02u8; 32];
        let mut l0 = [0u8; 64];
        l0[32..].copy_from_slice(&a);
        let h01 = sha256d(&l0);
        let mut l1 = [0u8; 64];
        l1[..32].copy_from_slice(&b);
        l1[32..].copy_from_slice(&b);
        let h22 = sha256d(&l1);
        let mut top = [0u8; 64];
        top[..32].copy_from_slice(&h01);
        top[32..].copy_from_slice(&h22);
        assert_eq!(witness_merkle_root(&[a, b]), sha256d(&top));
    }

    #[test]
    fn is_dnrw_script_recognises_and_rejects() {
        let good = build_dnrw_script_coinbase_only();
        assert!(is_dnrw_script(&good));
        // DNRF magic must not match.
        let mut dnrf = good.clone();
        dnrf[5] = 0x46; // 'F'
        assert!(!is_dnrw_script(&dnrf));
        assert!(!is_dnrw_script(&good[..38]));
    }

    #[test]
    fn mandatory_height_gate() {
        assert!(!requires_witness_commitment(10_669));
        assert!(requires_witness_commitment(10_670));
        assert!(requires_witness_commitment(61_410));
    }
}
