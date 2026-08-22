//! Regression test for the 2026-07-09 `bad-utreexo-root` outage.
//!
//! dinero-v8 commit `92e71dd5e` hard-forked utreexo leaf hashing at
//! mainnet height 60000 (`DINERO-UTXO-LEAF-v2`: authenticates
//! `created_height` + `is_coinbase`). The Rust stack kept hashing v1
//! leaves, so every pool/miner-recomputed header root past 60000 was
//! wrong and dinerod rejected every found block at accept time with
//! `bad-utreexo-root` (shares don't touch the daemon, so share
//! acceptance kept working — the outage was invisible until a share
//! met the block target).
//!
//! This test replays a snapshot captured from the live SJ mainnet
//! daemon (v8.0.13, height 61403): `getutreexoroots` pre-block forest
//! + the `getblocktemplate` coinbase. The daemon's own
//!   `utreexocommitment` for that template is the consensus-correct
//!   post-coinbase root, so it pins [`leaf_hash_v2`] to the C++
//!   implementation byte-for-byte. (`utreexocommitment` is a `GetHex()`
//!   display string — byte-reversed relative to the raw commitment.)

use dinero_sv2_jd::{
    commitment, leaf_hash, leaf_hash_for_height, UtreexoAccumulatorState,
    UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
};

const NUM_LEAVES: u64 = 184162;
const ROOTS: [&str; 10] = [
    "8244af0afe499ebafd123c58c30e2542f7024861bb75543da125a512e737f343",
    "70cba19242c67dff4146fc3438497b04710b9a851a43d67cfcb4a8331d291d5d",
    "8bfb2fc54040186028da4fb379e9b9c079c0bf332e1ed660bf96772fe2031b05",
    "e3f8b440a0ec99f1f446bed41f71940218e4215a6d4dfc10c4646ed780e775f3",
    "b1803904c6a112870646a0ec365d6577f5e16409a0180d72269c0c4a6a9043b8",
    "da18f4d1c040867f69e8a5763570e39a9215def5ca1b5c419860b2ae2bbe23e6",
    "7a313457be42a984f3df1a5360aa3fefeeafbc4318badb0c6f914e4e4a08d464",
    "fe35919a0a62f5a75806f535143a74b4f7e023cfe4fde3d61a590fd5d9906f6d",
    "c5e8e642221cada808f33bf0e254a27576fe4d68b3a006f928f9c5606277123b",
    "a5f65d72434b2d3db4f9c8bb691c9692dd1ede3c7cab575d01978fa3204a7325",
];
const HEIGHT: u32 = 61403;
const TXID_DISPLAY: &str = "2de0e93c2f65f4ea35ff809d65d5b686b76c12e154c7ea28d19e2c3213974468";
/// Daemon-reported `utreexocommitment` (display order) for this template.
const DAEMON_COMMITMENT_DISPLAY: &str =
    "c587b4a68f748003dbb324a3443b560f1c55a6d3685117224507e305bd163bae";
const OUTPUTS: [(u64, &str); 3] = [
    (
        10_000_000_000,
        "5120499c2aee9ac29d750af0ad6a752ab3c549b8c1862b3bc31a0091d9715a676bd8",
    ),
    (
        0,
        "6a25444e525701e2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf9",
    ),
    (
        0,
        "6a25444e52460104821b43a6c1f886bffff9aeeb1471fdd4f031d33d290735c756e4e41a293461",
    ),
];

fn hex32(s: &str) -> [u8; 32] {
    let b = hex::decode(s).unwrap();
    let mut a = [0u8; 32];
    a.copy_from_slice(&b);
    a
}

fn state() -> UtreexoAccumulatorState {
    UtreexoAccumulatorState {
        forest_roots: ROOTS.iter().map(|r| hex32(r)).collect(),
        num_leaves: NUM_LEAVES,
    }
}

fn txid_raw() -> [u8; 32] {
    let mut t = hex32(TXID_DISPLAY);
    t.reverse();
    t
}

fn daemon_commitment_raw() -> [u8; 32] {
    let mut c = hex32(DAEMON_COMMITMENT_DISPLAY);
    c.reverse();
    c
}

/// Post-coinbase root computed with the fixed, activation-aware leaf
/// hashing must reproduce the mainnet daemon's template commitment.
#[test]
fn v2_leaves_reproduce_live_mainnet_gbt_commitment() {
    let txid = txid_raw();
    let mut st = state();
    for (i, (val, spk_hex)) in OUTPUTS.iter().enumerate() {
        let spk = hex::decode(spk_hex).unwrap();
        st.add_leaf(leaf_hash_for_height(
            &txid,
            i as u32,
            *val,
            &spk,
            HEIGHT,
            true,
            UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
        ))
        .unwrap();
    }
    assert_eq!(
        commitment(&st).unwrap(),
        daemon_commitment_raw(),
        "activation-aware leaf hashing must match dinerod's post-fork root"
    );
}

/// The pre-fix behaviour (v1 leaves past activation) must NOT match —
/// this is the exact mismatch dinerod rejected as `bad-utreexo-root`.
#[test]
fn v1_leaves_do_not_match_post_fork_commitment() {
    let txid = txid_raw();
    let mut st = state();
    for (i, (val, spk_hex)) in OUTPUTS.iter().enumerate() {
        let spk = hex::decode(spk_hex).unwrap();
        st.add_leaf(leaf_hash(&txid, i as u32, *val, &spk)).unwrap();
    }
    assert_ne!(
        commitment(&st).unwrap(),
        daemon_commitment_raw(),
        "v1 leaves matching would mean the fork is not what broke block submission"
    );
}
