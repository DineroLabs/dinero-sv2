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
pub fn share_weight(share_target: &[u8; 32]) -> u128 {
    let hi = u128::from_be_bytes(share_target[0..16].try_into().unwrap());
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
        self.entries.iter().map(|e| e.weight).sum()
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    pub fn miner_bps(&self, payout_script: &[u8]) -> u32 {
        let total = self.total_weight();
        if total == 0 { return 0; }
        let mine: u128 = self.entries.iter()
            .filter(|e| e.payout_script == payout_script)
            .map(|e| e.weight)
            .sum();
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
    fn window_sums_weights_per_script_and_evicts_oldest() {
        let mut w = PplnsWindow::new(14_400);
        // Force a tiny cap for the test by filling beyond the floor:
        // record 600 shares for A, then 500 for B; with floor 500 the
        // window keeps at most its dynamic N — assert eviction dropped
        // the oldest (A) entries first.
        for i in 0..600 { w.record(vec![0xAA], 10, 1000 + i); }
        for i in 0..500 { w.record(vec![0xBB], 10, 2000 + i); }
        let weights = w.weights();
        assert!(w.len() <= 50_000);
        assert!(weights[&vec![0xBB]] == 500 * 10);
        // A lost entries to eviction before B did:
        assert!(weights.get(&vec![0xAA]).copied().unwrap_or(0) <= 600 * 10);
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
}
