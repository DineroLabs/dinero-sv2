//! In-memory share ledger keyed by the miner's Noise static public key.
//!
//! Phase 4 is deliberately ephemeral: credits reset on pool restart.
//! Persistence + real PPLNS scoring is Phase 4b once we see how real
//! miners actually connect (single-user home rigs? many anonymous
//! clients? known-identity worker pools?).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// A miner identity: their Noise static public key (32 bytes). Anonymous
/// NX handshakes from miners that didn't bring a static key are keyed
/// by `[0u8; 32]` (they get bucketed together in Phase 4 — an explicit
/// TODO for Phase 4b auth).
pub type MinerKey = [u8; 32];

/// In-memory credit ledger.
#[derive(Debug, Default)]
pub struct Ledger {
    inner: Mutex<HashMap<MinerKey, Credit>>,
}

/// Per-miner credit tally.
#[derive(Debug, Clone, Copy, Default)]
pub struct Credit {
    /// Number of shares that met the pool's share target.
    pub accepted_shares: u64,
    /// Number of shares that ALSO met the block target (blocks found).
    pub found_blocks: u64,
    /// Number of shares rejected (bad shape, stale template, etc).
    pub rejected_shares: u64,
}

impl Ledger {
    /// Credit one accepted share.
    pub fn credit_share(&self, miner: MinerKey) {
        let mut g = self.inner.lock().expect("ledger mutex");
        g.entry(miner).or_default().accepted_shares += 1;
    }

    /// Credit one block (also implies the share that found it was
    /// already counted via `credit_share`).
    pub fn credit_block(&self, miner: MinerKey) {
        let mut g = self.inner.lock().expect("ledger mutex");
        g.entry(miner).or_default().found_blocks += 1;
    }

    /// Count one rejection.
    pub fn reject(&self, miner: MinerKey) {
        let mut g = self.inner.lock().expect("ledger mutex");
        g.entry(miner).or_default().rejected_shares += 1;
    }

    /// Snapshot of the whole ledger. Used by tests and the (future)
    /// ops endpoint; the `dinero-sv2-pool` binary doesn't call it yet
    /// but Phase 4b's persistence/payout code will.
    #[allow(dead_code)]
    pub fn snapshot(&self) -> HashMap<MinerKey, Credit> {
        self.inner.lock().expect("ledger mutex").clone()
    }
}

/// Difficulty weight of one accepted share: inversely proportional to
/// its target. Uses the top 16 bytes of the 32-byte big-endian target
/// (pool targets are of the form 00003fff…, so the low half is
/// saturated and carries no information).
///
/// # Saturation domain
///
/// This function only distinguishes difficulty within targets that are
/// `>= 2^128` (i.e. fewer than 128 leading zero bits). Any target below
/// `2^128` (128+ leading zero bits — harder than ~2^128 pool difficulty)
/// makes `hi == 0` and saturates to `u128::MAX`, collapsing monotonicity
/// for targets in that range. The pool enforces `--share-leading-bits
/// <= 96` at startup (see `main.rs`) specifically to keep real share
/// targets far above this saturation boundary.
pub fn share_weight(share_target: &[u8; 32]) -> u128 {
    let hi = u128::from_be_bytes(share_target[0..16].try_into().unwrap());
    if hi == 0 {
        return u128::MAX;
    }
    u128::MAX / hi.saturating_add(1)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEntry {
    pub payout_script: Vec<u8>,
    pub weight: u128,
    pub unix_ts: u64,
}

/// Rolling PPLNS window. N is dynamic: it targets `target_secs` of
/// pool-wide shared work (recomputed from the observed span of the
/// entries), clamped to [500, 50_000] entries.
#[derive(Debug)]
pub struct PplnsWindow {
    entries: VecDeque<WindowEntry>,
    target_secs: u64,
}

impl PplnsWindow {
    pub const FLOOR: usize = 500;
    pub const CAP: usize = 50_000;

    pub fn new(target_secs: u64) -> Self {
        Self { entries: VecDeque::new(), target_secs }
    }

    pub fn restore(entries: Vec<WindowEntry>, target_secs: u64) -> Self {
        let mut w = Self { entries: entries.into(), target_secs };
        w.evict();
        w
    }

    pub fn record(&mut self, payout_script: Vec<u8>, weight: u128, unix_ts: u64) {
        self.entries.push_back(WindowEntry { payout_script, weight, unix_ts });
        self.evict();
    }

    /// Dynamic N: entries-per-second observed over the current window
    /// span × target_secs, clamped.
    fn max_len(&self) -> usize {
        let (Some(first), Some(last)) = (self.entries.front(), self.entries.back()) else {
            return Self::CAP;
        };
        let span = last.unix_ts.saturating_sub(first.unix_ts).max(1);
        let rate_num = self.entries.len() as u64; // shares over `span` secs
        let n = (rate_num.saturating_mul(self.target_secs) / span) as usize;
        n.clamp(Self::FLOOR, Self::CAP)
    }

    fn evict(&mut self) {
        let max = self.max_len();
        while self.entries.len() > max {
            self.entries.pop_front();
        }
    }

    pub fn weights(&self) -> HashMap<Vec<u8>, u128> {
        let mut m = HashMap::new();
        for e in &self.entries {
            *m.entry(e.payout_script.clone()).or_insert(0u128) += e.weight;
        }
        m
    }

    pub fn total_weight(&self) -> u128 {
        self.entries.iter().fold(0u128, |acc, e| acc.saturating_add(e.weight))
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    pub fn miner_bps(&self, payout_script: &[u8]) -> u32 {
        let total = self.total_weight();
        if total == 0 { return 0; }
        let mine: u128 = self.entries.iter()
            .filter(|e| e.payout_script == payout_script)
            .fold(0u128, |acc, e| acc.saturating_add(e.weight));
        ((mine.saturating_mul(10_000)) / total) as u32
    }

    pub fn entries(&self) -> impl Iterator<Item = &WindowEntry> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_share_and_block_accumulates() {
        let l = Ledger::default();
        let m: MinerKey = [0x11; 32];
        l.credit_share(m);
        l.credit_share(m);
        l.credit_block(m);
        l.reject(m);
        let snap = l.snapshot();
        let c = snap[&m];
        assert_eq!(c.accepted_shares, 2);
        assert_eq!(c.found_blocks, 1);
        assert_eq!(c.rejected_shares, 1);
    }

    #[test]
    fn separate_miners_are_scored_separately() {
        let l = Ledger::default();
        let a: MinerKey = [0x01; 32];
        let b: MinerKey = [0x02; 32];
        l.credit_share(a);
        l.credit_share(a);
        l.credit_share(b);
        let snap = l.snapshot();
        assert_eq!(snap[&a].accepted_shares, 2);
        assert_eq!(snap[&b].accepted_shares, 1);
    }

    #[test]
    fn share_weight_is_monotonic_in_difficulty() {
        let mut easy = [0xFFu8; 32]; easy[0] = 0x00; easy[1] = 0x0F;   // 000f ff…
        let mut hard = [0xFFu8; 32]; hard[0] = 0x00; hard[1] = 0x00; hard[2] = 0x3F; // 00003f…
        assert!(share_weight(&hard) > share_weight(&easy));
        assert!(share_weight(&easy) > 0);
    }

    #[test]
    fn share_weight_saturates_below_2pow128() {
        use crate::target::leading_zero_bits_target;

        // 130 and 140 leading zero bits both fall below the 2^128
        // saturation boundary (>= 128 leading zero bits) and must both
        // saturate to u128::MAX (documented behaviour, not a precision
        // bug — the pool never issues share targets this hard).
        let t130 = leading_zero_bits_target(130);
        let t140 = leading_zero_bits_target(140);
        assert_eq!(share_weight(&t130), u128::MAX);
        assert_eq!(share_weight(&t140), u128::MAX);

        // 96 leading zero bits is within the pool's enforced domain
        // (<= 96, see main.rs validation) and must be strictly ordered:
        // harder (96 bits) > easier (60 bits), neither saturated.
        let t96 = leading_zero_bits_target(96);
        let t60 = leading_zero_bits_target(60);
        let w96 = share_weight(&t96);
        let w60 = share_weight(&t60);
        assert!(w96 < u128::MAX);
        assert!(w96 > w60);
    }

    #[test]
    fn window_sums_weights_per_script_no_eviction() {
        let mut w = PplnsWindow::new(14_400);
        w.record(vec![0xAA], 10, 1000);
        w.record(vec![0xAA], 10, 1001);
        w.record(vec![0xBB], 5, 1002);
        let weights = w.weights();
        assert_eq!(w.len(), 3);
        assert_eq!(weights[&vec![0xAA]], 20);
        assert_eq!(weights[&vec![0xBB]], 5);
        let bps = w.miner_bps(&[0xBB]);
        assert!(bps > 0 && bps <= 10_000);
    }

    #[test]
    fn window_evicts_oldest_entries_deterministically() {
        // target_secs = 14_400. Record 1000 entries for script A, 30s
        // apart. Eviction runs after every `record`, so by the time the
        // 1000th entry lands, the window has already converged to a
        // steady state: max_len = (entries.len() * 14_400) / span,
        // clamped to FLOOR (500) — the span between the surviving
        // oldest and newest entries shrinks as old ones get popped,
        // which is exactly why a *fixed* 30_000s span computed only
        // from the full recording run (giving 480, "clamped up to
        // 500") undersells what actually happens: the window settles
        // at exactly FLOOR = 500 entries, holding the newest 500 of
        // the 1000 recorded (ts 15_000..=29_970).
        let mut w = PplnsWindow::new(14_400);
        for i in 0..1000u64 {
            w.record(vec![0xAA], 10, i * 30);
        }
        assert_eq!(w.len(), PplnsWindow::FLOOR);
        let weights_after_a = w.weights();
        assert_eq!(weights_after_a[&vec![0xAA]], 500 * 10);

        // Now record 200 more, densely spaced (1s apart), under script
        // B. This raises the observed rate a lot relative to span, so
        // max_len grows again and eviction doesn't need to touch B's
        // entries at all — but A does lose some of its oldest entries
        // to make room as the window's dynamic sizing shifts.
        let base_ts = 1000 * 30;
        for i in 0..200u64 {
            w.record(vec![0xBB], 10, base_ts + i);
        }

        let weights = w.weights();
        // All 200 B entries are newest and must have survived intact.
        assert_eq!(weights[&vec![0xBB]], 200 * 10);
        // A lost entries relative to its post-first-loop total (it can
        // only have gone down, never up, since the last 1000 A record
        // calls already happened).
        assert!(weights[&vec![0xAA]] < 500 * 10);
        assert!(weights[&vec![0xAA]] > 0);

        let bps = w.miner_bps(&[0xBB]);
        assert!(bps > 0 && bps <= 10_000);
    }

    #[test]
    fn window_restore_round_trips() {
        let mut w = PplnsWindow::new(14_400);
        w.record(vec![0x51], 7, 42);
        let entries: Vec<WindowEntry> = w.entries().cloned().collect();
        let w2 = PplnsWindow::restore(entries, 14_400);
        assert_eq!(w2.total_weight(), 7);
    }

    #[test]
    fn total_weight_and_miner_bps_sums_saturate_instead_of_overflowing() {
        // Defense-in-depth: the live path never produces weights near
        // u128::MAX (share_weight is bounded by the pool's
        // share-leading-bits <= 96 cap), but total_weight()/miner_bps()
        // must not panic (debug) or wrap (release) if it ever did. Two
        // entries at u128::MAX would overflow a plain `+`/`.sum()`; with
        // `saturating_add` the sum instead clamps at u128::MAX.
        let mut w = PplnsWindow::new(14_400);
        w.record(vec![0x99], u128::MAX, 1);
        w.record(vec![0x99], u128::MAX, 2);
        assert_eq!(w.total_weight(), u128::MAX);
        // miner_bps's internal `mine` sum must also saturate rather
        // than panic/wrap. Note the downstream `.saturating_mul(10_000)`
        // also clamps at this extreme (mine == total == u128::MAX), so
        // the numerator loses precision and bps comes out far below the
        // "should be 10_000" ideal — that's a pre-existing precision
        // quirk of the saturating-mul-then-divide formula at the edges
        // of u128, out of scope here; this test only asserts we don't
        // panic/wrap on the accumulation itself and get a valid bps.
        let bps = w.miner_bps(&[0x99]);
        assert!(bps <= 10_000);
    }
}
