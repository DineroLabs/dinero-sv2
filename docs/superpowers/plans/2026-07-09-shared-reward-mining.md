# Shared-Reward Mining (PPLNS) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Opt-in-per-phone shared mining: the pool builds PPLNS-split coinbases so many phones mine together and every found block pays recent contributors directly on-chain (trustless — pool never holds funds).

**Architecture:** Shared-mode miners mine pool-owned jobs (pool assembles coinbase = split outputs + 2% fee + DNRW + DNRF, computes merkle + v2 utreexo root) and submit standard shares; solo mode (extended/JD shares, miner-owned coinbase) is untouched. Difficulty-weighted PPLNS window, persisted as JSONL. Spec: `docs/superpowers/specs/2026-07-09-shared-reward-mining-design.md`.

**Tech Stack:** Rust (dinero-sv2 workspace, dinero-rust/dpi FFI), Swift (DineroDPI), SV2 wire protocol over Noise NX.

## Global Constraints

- Fee default **200 bps (2%)** to the fleet payout script; flags: `--shared-fee-bps` (default 200), `--shared-max-outputs` (default 20), `--shared-dust-una` (default 10_000), `--pplns-journal` (default `/var/lib/dinero-sv2/pplns-journal.jsonl`).
- PPLNS window: dynamic N covering ~14_400 s of pool shared work; floor 500 entries, cap 50_000.
- New wire messages: `MSG_SET_REWARD_MODE = 0x23`, `MSG_WINDOW_STATUS = 0x24` (0x00–0x22 are taken; see `crates/dinero-sv2-transport/src/lib.rs`).
- A miner that never sends `SetRewardMode` is SOLO — existing clients must keep working byte-for-byte.
- Every coinbase invariant: `sum(output values) == coinbase_value_una`; DNRF filter must commit to **all** value-output scripts of the shared coinbase (`gcs_build(prev_hash_le, &[script, script, …])`), not just one payout.
- Utreexo leaves for the shared coinbase use `leaf_hash_for_height(…, height, true, UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET)`.
- Deploys to SJ: `rsync -az --exclude target --exclude .git` — **never `--delete`** (see 2026-07-09 key-loss incident; key lives in `/etc/dinero-sv2/pool-static.key`).
- Commit style: `feat(scope): …` / `fix(scope): …`, each commit ends with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

## Phase A — pool (`~/src/dinero-sv2`), branch `feat/shared-reward-pplns`

### Task 1: Wire messages `SetRewardMode` + `WindowStatus`

**Files:**
- Modify: `crates/dinero-sv2-transport/src/lib.rs` (two constants)
- Modify: `crates/dinero-sv2-common/src/sv2_messages.rs` (two structs)
- Modify: `crates/dinero-sv2-codec/src/sv2.rs` (encode/decode + tests)

**Interfaces:**
- Produces: `SetRewardMode { channel_id: u32, mode: u8, payout_script: Vec<u8> }`, `WindowStatus { channel_id: u32, window_bps: u32, window_shares: u64 }`; `encode_set_reward_mode/decode_set_reward_mode`, `encode_window_status/decode_window_status` (same `Result<_, Sv2CodecError>` shape as the existing pairs in `sv2.rs`).

- [ ] **Step 1: Add message-ID constants** in `crates/dinero-sv2-transport/src/lib.rs`, after `MSG_SET_TARGET`:

```rust
/// Miner → pool (Dinero extension): declare reward mode for this
/// channel. mode 0 = solo (default when never sent), 1 = shared.
/// Carries the payout script the PPLNS ledger credits.
pub const MSG_SET_REWARD_MODE: u8 = 0x23;
/// Pool → miner (Dinero extension): the miner's current PPLNS window
/// standing, for UI display.
pub const MSG_WINDOW_STATUS: u8 = 0x24;
```

- [ ] **Step 2: Add structs** in `crates/dinero-sv2-common/src/sv2_messages.rs` (bottom, near `SubmitSharesExtendedDinero`):

```rust
/// Miner → pool: reward-mode declaration (Dinero extension, 0x23).
/// Sent once, after OpenStandardMiningChannel.Success. A channel that
/// never sends it mines SOLO (backward compatible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRewardMode {
    pub channel_id: u32,
    /// 0 = solo, 1 = shared.
    pub mode: u8,
    /// Payout scriptPubKey credited by the PPLNS ledger (34-byte
    /// taproot expected; pool validates shape).
    pub payout_script: Vec<u8>,
}

/// Pool → miner: PPLNS window standing (Dinero extension, 0x24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowStatus {
    pub channel_id: u32,
    /// The miner's fraction of the current window in basis points.
    pub window_bps: u32,
    /// Total shares currently in the window (pool-wide).
    pub window_shares: u64,
}
```

- [ ] **Step 3: Write failing codec tests** in `crates/dinero-sv2-codec/src/sv2.rs` tests module (mirror the style of `submit_shares_error_roundtrip`):

```rust
#[test]
fn set_reward_mode_roundtrip() {
    let msg = SetRewardMode {
        channel_id: 7,
        mode: 1,
        payout_script: vec![0x51, 0x20, 0xAB],
    };
    let buf = encode_set_reward_mode(&msg).unwrap();
    assert_eq!(decode_set_reward_mode(&buf).unwrap(), msg);
}

#[test]
fn set_reward_mode_rejects_truncated() {
    let msg = SetRewardMode { channel_id: 1, mode: 0, payout_script: vec![0x51] };
    let buf = encode_set_reward_mode(&msg).unwrap();
    assert!(decode_set_reward_mode(&buf[..buf.len() - 1]).is_err());
}

#[test]
fn window_status_roundtrip() {
    let msg = WindowStatus { channel_id: 9, window_bps: 314, window_shares: 4096 };
    let buf = encode_window_status(&msg).unwrap();
    assert_eq!(decode_window_status(&buf).unwrap(), msg);
}
```

- [ ] **Step 4: Run to verify failure** — `cargo test -p dinero-sv2-codec set_reward_mode window_status` → compile error (functions/structs unresolved).

- [ ] **Step 5: Implement encode/decode** in `crates/dinero-sv2-codec/src/sv2.rs`, following the existing byte conventions in that file (u32 LE, u64 LE, `write_bytes_u16`-style length-prefixed script — reuse the file's existing helpers; check how `SubmitSharesExtendedDinero` encodes `coinbase_outputs` and copy that length-prefix pattern):

```rust
pub fn encode_set_reward_mode(msg: &SetRewardMode) -> Result<Vec<u8>, Sv2CodecError> {
    let mut buf = Vec::with_capacity(4 + 1 + 2 + msg.payout_script.len());
    buf.extend_from_slice(&msg.channel_id.to_le_bytes());
    buf.push(msg.mode);
    let len = u16::try_from(msg.payout_script.len())
        .map_err(|_| Sv2CodecError::FieldTooLong("payout_script"))?;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&msg.payout_script);
    Ok(buf)
}

pub fn decode_set_reward_mode(buf: &[u8]) -> Result<SetRewardMode, Sv2CodecError> {
    if buf.len() < 7 { return Err(Sv2CodecError::Truncated("SetRewardMode")); }
    let channel_id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let mode = buf[4];
    let len = u16::from_le_bytes(buf[5..7].try_into().unwrap()) as usize;
    if buf.len() != 7 + len { return Err(Sv2CodecError::Truncated("SetRewardMode.payout_script")); }
    Ok(SetRewardMode { channel_id, mode, payout_script: buf[7..7 + len].to_vec() })
}

pub fn encode_window_status(msg: &WindowStatus) -> Result<Vec<u8>, Sv2CodecError> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&msg.channel_id.to_le_bytes());
    buf.extend_from_slice(&msg.window_bps.to_le_bytes());
    buf.extend_from_slice(&msg.window_shares.to_le_bytes());
    Ok(buf)
}

pub fn decode_window_status(buf: &[u8]) -> Result<WindowStatus, Sv2CodecError> {
    if buf.len() != 16 { return Err(Sv2CodecError::Truncated("WindowStatus")); }
    Ok(WindowStatus {
        channel_id: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        window_bps: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        window_shares: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
    })
}
```

Adapt error-variant names to what `Sv2CodecError` actually offers (see the enum at the top of `sv2.rs`); if there is no `FieldTooLong`/`Truncated`, use the closest existing variants rather than adding new ones.

- [ ] **Step 6: Run to verify pass** — `cargo test -p dinero-sv2-codec set_reward_mode window_status` → 3 passed. Also `cargo test -p dinero-sv2-codec` (no regressions).

- [ ] **Step 7: Commit** — `feat(codec): SetRewardMode + WindowStatus wire messages (0x23/0x24)`

### Task 2: PPLNS window in `accounting.rs`

**Files:**
- Modify: `crates/dinero-sv2-pool/src/accounting.rs` (keep `Ledger` as-is; add below it)

**Interfaces:**
- Produces:
  - `pub fn share_weight(share_target: &[u8; 32]) -> u128` — monotonic difficulty weight.
  - `pub struct PplnsWindow` with `pub fn new(target_secs: u64) -> Self`, `pub fn record(&mut self, payout_script: Vec<u8>, weight: u128, unix_ts: u64)`, `pub fn weights(&self) -> HashMap<Vec<u8>, u128>` (per-script sums), `pub fn total_weight(&self) -> u128`, `pub fn len(&self) -> usize`, `pub fn miner_bps(&self, payout_script: &[u8]) -> u32`, `pub fn entries(&self) -> impl Iterator<Item = &WindowEntry>` (for journaling), `pub fn restore(entries: Vec<WindowEntry>, target_secs: u64) -> Self`.
  - `pub struct WindowEntry { pub payout_script: Vec<u8>, pub weight: u128, pub unix_ts: u64 }` (serde Serialize/Deserialize).

- [ ] **Step 1: Write failing tests** (same file, tests module):

```rust
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
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p dinero-sv2-pool window_ share_weight` → unresolved symbols.

- [ ] **Step 3: Implement** in `accounting.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

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

    pub fn weights(&self) -> std::collections::HashMap<Vec<u8>, u128> {
        let mut m = std::collections::HashMap::new();
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
```

Add `serde = { workspace = true, features = ["derive"] }` to `crates/dinero-sv2-pool/Cargo.toml` dependencies if not already present (check first — the pool already uses `serde_json` for RPC).

- [ ] **Step 4: Run to verify pass** — `cargo test -p dinero-sv2-pool window_ share_weight` → 3 passed.

- [ ] **Step 5: Commit** — `feat(pool): difficulty-weighted PPLNS window`

### Task 3: Window journal (JSONL persistence)

**Files:**
- Create: `crates/dinero-sv2-pool/src/journal.rs`
- Modify: `crates/dinero-sv2-pool/src/main.rs` (add `mod journal;`)

**Interfaces:**
- Produces: `pub struct WindowJournal` with `pub fn open(path: &Path) -> anyhow::Result<Self>`, `pub fn append(&mut self, entry: &WindowEntry) -> anyhow::Result<()>` (line-buffered JSONL), `pub fn load(path: &Path) -> anyhow::Result<Vec<WindowEntry>>` (skips corrupt lines with a warn), `pub fn compact(&mut self, live: &PplnsWindow) -> anyhow::Result<()>` (rewrite file to only the live entries; call every 10_000 appends).

- [ ] **Step 1: Write failing tests** (in `journal.rs` tests module, using `tempfile` — add `tempfile = "3"` to `[dev-dependencies]`):

```rust
#[test]
fn journal_round_trip_and_corrupt_line_skip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.jsonl");
    let mut j = WindowJournal::open(&path).unwrap();
    j.append(&WindowEntry { payout_script: vec![0x51], weight: 5, unix_ts: 1 }).unwrap();
    j.append(&WindowEntry { payout_script: vec![0x52], weight: 6, unix_ts: 2 }).unwrap();
    drop(j);
    // Corrupt the middle of the file:
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.push_str("{not json\n");
    std::fs::write(&path, raw).unwrap();
    let loaded = WindowJournal::load(&path).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[1].weight, 6);
}

#[test]
fn compact_rewrites_to_live_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.jsonl");
    let mut j = WindowJournal::open(&path).unwrap();
    for i in 0..10 {
        j.append(&WindowEntry { payout_script: vec![0x51], weight: 1, unix_ts: i }).unwrap();
    }
    let mut w = PplnsWindow::new(14_400);
    w.record(vec![0x51], 1, 9);
    j.compact(&w).unwrap();
    assert_eq!(WindowJournal::load(&path).unwrap().len(), 1);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p dinero-sv2-pool journal` → unresolved.

- [ ] **Step 3: Implement** `journal.rs`:

```rust
//! Append-only JSONL persistence for the PPLNS window. Losing this
//! file never risks funds — only unpaid share *credit* (the window
//! rebuilds from new shares).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::accounting::{PplnsWindow, WindowEntry};

pub struct WindowJournal {
    path: PathBuf,
    writer: BufWriter<File>,
    appends_since_compact: u64,
}

impl WindowJournal {
    pub const COMPACT_EVERY: u64 = 10_000;

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("mkdir {dir:?}"))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)
            .with_context(|| format!("open journal {path:?}"))?;
        Ok(Self { path: path.to_path_buf(), writer: BufWriter::new(file), appends_since_compact: 0 })
    }

    pub fn append(&mut self, entry: &WindowEntry) -> Result<()> {
        serde_json::to_writer(&mut self.writer, entry)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.appends_since_compact += 1;
        Ok(())
    }

    pub fn should_compact(&self) -> bool {
        self.appends_since_compact >= Self::COMPACT_EVERY
    }

    pub fn load(path: &Path) -> Result<Vec<WindowEntry>> {
        if !path.exists() { return Ok(Vec::new()); }
        let reader = BufReader::new(File::open(path)?);
        let mut out = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<WindowEntry>(&line) {
                Ok(e) => out.push(e),
                Err(e) => warn!(line = i, error = %e, "skipping corrupt journal line"),
            }
        }
        Ok(out)
    }

    pub fn compact(&mut self, live: &PplnsWindow) -> Result<()> {
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for e in live.entries() {
                serde_json::to_writer(&mut w, e)?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.writer = BufWriter::new(file);
        self.appends_since_compact = 0;
        Ok(())
    }
}
```

- [ ] **Step 4: Run to verify pass** — `cargo test -p dinero-sv2-pool journal` → 2 passed.

- [ ] **Step 5: Commit** — `feat(pool): PPLNS window JSONL journal`

### Task 4: Split engine

**Files:**
- Create: `crates/dinero-sv2-pool/src/split.rs`
- Modify: `crates/dinero-sv2-pool/src/main.rs` (add `mod split;`)

**Interfaces:**
- Consumes: `PplnsWindow::weights()` output (`HashMap<Vec<u8>, u128>`).
- Produces:
```rust
pub struct SplitParams<'a> {
    pub reward_una: u64,
    pub fee_bps: u32,           // e.g. 200
    pub fee_script: &'a [u8],   // fleet payout scriptPubKey
    pub max_outputs: usize,     // e.g. 20 (contributor outputs, excl. fee)
    pub dust_una: u64,          // e.g. 10_000
    pub finder_script: &'a [u8],
}
pub fn compute_split(
    weights: &HashMap<Vec<u8>, u128>,
    p: &SplitParams,
) -> Vec<CoinbaseOutput>   // dinero_sv2_jd::CoinbaseOutput { value_una, script_pubkey }
```
Guarantees: `sum(value_una) == reward_una` always; deterministic ordering (weight desc, script asc tiebreak); fee output always present (absorbs dust/rounding when finder absent); empty window → `reward - fee` to finder.

- [ ] **Step 1: Write failing tests** (`split.rs` tests module):

```rust
fn s(b: u8) -> Vec<u8> { vec![0x51, 0x20, b] }

#[test]
fn split_sums_to_reward_with_fee() {
    let mut w = HashMap::new();
    w.insert(s(1), 300u128);
    w.insert(s(2), 100u128);
    let p = SplitParams { reward_una: 10_000_000_000, fee_bps: 200, fee_script: &s(9),
        max_outputs: 20, dust_una: 10_000, finder_script: &s(1) };
    let outs = compute_split(&w, &p);
    assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), 10_000_000_000);
    let fee = outs.iter().find(|o| o.script_pubkey == s(9)).unwrap();
    assert!(fee.value_una >= 200_000_000); // ≥2% (may absorb rounding)
    let a = outs.iter().find(|o| o.script_pubkey == s(1)).unwrap();
    let b = outs.iter().find(|o| o.script_pubkey == s(2)).unwrap();
    assert!(a.value_una > b.value_una * 2); // ~3:1 plus rounding to finder
}

#[test]
fn split_caps_outputs_and_drops_dust() {
    let mut w = HashMap::new();
    for i in 0..30 { w.insert(s(i), 100u128); }
    w.insert(s(200), 1u128); // will be dust
    let p = SplitParams { reward_una: 10_000_000_000, fee_bps: 200, fee_script: &s(255),
        max_outputs: 20, dust_una: 10_000_000, finder_script: &s(0) };
    let outs = compute_split(&w, &p);
    // ≤ 20 contributor outputs + 1 fee output:
    assert!(outs.len() <= 21);
    assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), 10_000_000_000);
    assert!(!outs.iter().any(|o| o.script_pubkey == s(200)));
}

#[test]
fn empty_window_pays_finder_minus_fee() {
    let w = HashMap::new();
    let p = SplitParams { reward_una: 10_000_000_000, fee_bps: 200, fee_script: &s(9),
        max_outputs: 20, dust_una: 10_000, finder_script: &s(7) };
    let outs = compute_split(&w, &p);
    assert_eq!(outs.len(), 2);
    assert_eq!(outs.iter().find(|o| o.script_pubkey == s(7)).unwrap().value_una, 9_800_000_000);
    assert_eq!(outs.iter().find(|o| o.script_pubkey == s(9)).unwrap().value_una, 200_000_000);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p dinero-sv2-pool split` → unresolved.

- [ ] **Step 3: Implement** `split.rs`:

```rust
//! Trustless PPLNS coinbase split. Pure math — no I/O, no custody.
//! Dust and over-cap contributors are simply not paid THIS block;
//! their window credit remains and pays out from future blocks.

use std::collections::HashMap;

use dinero_sv2_jd::CoinbaseOutput;

pub struct SplitParams<'a> {
    pub reward_una: u64,
    pub fee_bps: u32,
    pub fee_script: &'a [u8],
    pub max_outputs: usize,
    pub dust_una: u64,
    pub finder_script: &'a [u8],
}

pub fn compute_split(weights: &HashMap<Vec<u8>, u128>, p: &SplitParams) -> Vec<CoinbaseOutput> {
    let fee_una = (u128::from(p.reward_una) * u128::from(p.fee_bps) / 10_000) as u64;
    let pot = p.reward_una - fee_una;

    // Deterministic order: weight desc, then script asc.
    let mut ranked: Vec<(&Vec<u8>, u128)> =
        weights.iter().map(|(k, v)| (k, *v)).filter(|(_, w)| *w > 0).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(p.max_outputs);

    let elected_total: u128 = ranked.iter().map(|(_, w)| *w).sum();

    let mut outs: Vec<CoinbaseOutput> = Vec::new();
    let mut paid: u64 = 0;
    if elected_total > 0 {
        for (script, w) in &ranked {
            let v = ((u128::from(pot) * w) / elected_total) as u64;
            if v >= p.dust_una {
                outs.push(CoinbaseOutput { value_una: v, script_pubkey: (*script).clone() });
                paid += v;
            }
        }
    }
    if outs.is_empty() {
        // Empty window (or everything dusted): finder takes the pot.
        outs.push(CoinbaseOutput { value_una: pot, script_pubkey: p.finder_script.to_vec() });
        paid = pot;
    }

    // Remainder (rounding + dusted slices) → finder's output if present,
    // else the fee output.
    let remainder = pot - paid;
    if remainder > 0 {
        if let Some(f) = outs.iter_mut().find(|o| o.script_pubkey == p.finder_script) {
            f.value_una += remainder;
        } else {
            // absorbed by fee below
        }
    }
    let fee_total = if remainder > 0
        && !outs.iter().any(|o| o.script_pubkey == p.finder_script)
    {
        fee_una + remainder
    } else {
        fee_una
    };
    outs.push(CoinbaseOutput { value_una: fee_total, script_pubkey: p.fee_script.to_vec() });

    debug_assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), p.reward_una);
    outs
}
```

- [ ] **Step 4: Run to verify pass** — `cargo test -p dinero-sv2-pool split` → 3 passed.

- [ ] **Step 5: Commit** — `feat(pool): PPLNS coinbase split engine`

### Task 5: Shared template builder

**Files:**
- Create: `crates/dinero-sv2-pool/src/shared_template.rs`
- Modify: `crates/dinero-sv2-pool/src/main.rs` (add `mod shared_template;`)

**Interfaces:**
- Consumes: `PoolTemplate` (mapper.rs — fields `coinbase_prefix`, `coinbase_suffix`, `coinbase_witness_bytes`, `utreexo_pre_block`, `height`, `coinbase_value_una`, `wire`), `compute_split` outputs, jd crate primitives.
- Produces:
```rust
pub struct SharedTemplate {
    pub wire: NewTemplateDinero,        // merkle_root + utreexo_root pool-computed
    pub coinbase_full_hex: String,      // segwit-wrapped, for submitblock
    pub outputs: Vec<CoinbaseOutput>,   // the split (for logs/audit)
}
pub fn build_shared_template(
    pt: &PoolTemplate,
    split_outputs: Vec<CoinbaseOutput>, // value outputs only (contributors + fee)
) -> anyhow::Result<SharedTemplate>
```

- [ ] **Step 1: Write failing test** (`shared_template.rs` tests module). Build a synthetic `PoolTemplate` the way `mapper::tests::fixture()` does (reuse `map_template` on the fixture JSON — see `crates/dinero-sv2-pool/src/mapper.rs` tests for the fixture; if the fixture lacks `utreexo_pre_block`, construct the parts directly):

```rust
#[test]
fn shared_template_coinbase_and_roots_are_consistent() {
    let pt = crate::mapper::tests::fixture_pool_template(); // make the existing test fixture pub(crate)
    let outputs = vec![
        CoinbaseOutput { value_una: pt.coinbase_value_una - 200_000_000, script_pubkey: vec![0x51, 0x20, 0x01] },
        CoinbaseOutput { value_una: 200_000_000, script_pubkey: vec![0x51, 0x20, 0x09] },
    ];
    let st = build_shared_template(&pt, outputs.clone()).unwrap();

    // Value invariant:
    let cb = hex::decode(&st.coinbase_full_hex).unwrap();
    assert!(cb.len() > 100);
    // Roots differ from the daemon-template ones (different coinbase):
    assert_ne!(st.wire.merkle_root, pt.wire.merkle_root);
    assert_ne!(st.wire.utreexo_root, pt.wire.utreexo_root);
    // Recompute root independently with v2 leaves and compare:
    let full_outputs = st.outputs.clone(); // value outputs + DNRW + DNRF
    let (_, txid) = assemble_stripped_coinbase(&pt.coinbase_prefix, &full_outputs, &pt.coinbase_suffix);
    let mut state = pt.utreexo_pre_block.clone().unwrap();
    for (i, o) in full_outputs.iter().enumerate() {
        state.add_leaf(leaf_hash_for_height(&txid, i as u32, o.value_una, &o.script_pubkey,
            pt.height, true, UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET)).unwrap();
    }
    assert_eq!(st.wire.utreexo_root, commitment(&state).unwrap());
}
```

(The `st.outputs` field must therefore contain the FULL output list including DNRW/DNRF — adjust the struct doc accordingly.)

- [ ] **Step 2: Run to verify failure** — `cargo test -p dinero-sv2-pool shared_template` → unresolved.

- [ ] **Step 3: Implement** `shared_template.rs`:

```rust
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

use crate::mapper::PoolTemplate;

pub struct SharedTemplate {
    pub wire: NewTemplateDinero,
    pub coinbase_full_hex: String,
    /// Full output list as assembled: value outputs, then DNRW, then DNRF.
    pub outputs: Vec<CoinbaseOutput>,
}

pub fn build_shared_template(
    pt: &PoolTemplate,
    split_outputs: Vec<CoinbaseOutput>,
) -> Result<SharedTemplate> {
    let value_sum: u64 = split_outputs.iter().map(|o| o.value_una).sum();
    if value_sum != pt.coinbase_value_una {
        return Err(anyhow!(
            "split sum {value_sum} != coinbase value {}", pt.coinbase_value_una
        ));
    }
    if !pt.mempool_txs.is_empty() {
        // Shared jobs are coinbase-only for now (spec: out of scope).
        return Err(anyhow!("shared templates are coinbase-only"));
    }
    let pre_block = pt.utreexo_pre_block.as_ref()
        .ok_or_else(|| anyhow!("template lacks utreexo pre-block state"))?;

    let mut outputs = split_outputs;

    // DNRW (coinbase-only constant), then DNRF committing to ALL value
    // output scripts — the daemon rebuilds the block filter from every
    // output at accept time, so a single-payout filter would be
    // rejected as bad-dnrf.
    if requires_witness_commitment(pt.height as u64) {
        outputs.push(CoinbaseOutput {
            value_una: 0,
            script_pubkey: build_dnrw_script_coinbase_only(),
        });
    }
    if requires_filter_commitment(pt.height as u64) {
        let script_refs: Vec<&[u8]> = outputs
            .iter()
            .filter(|o| o.value_una > 0)
            .map(|o| o.script_pubkey.as_slice())
            .collect();
        let (encoded_filter, _) = gcs_build(&pt.wire.prev_block_hash, &script_refs);
        let dnrf = build_dnrf_script(&gcs_filter_hash(&encoded_filter));
        outputs.push(CoinbaseOutput { value_una: 0, script_pubkey: dnrf });
    }

    let (coinbase_stripped, coinbase_txid) =
        assemble_stripped_coinbase(&pt.coinbase_prefix, &outputs, &pt.coinbase_suffix);

    let mut state = pre_block.clone();
    for (i, o) in outputs.iter().enumerate() {
        state.add_leaf(leaf_hash_for_height(
            &coinbase_txid, i as u32, o.value_una, &o.script_pubkey,
            pt.height, true, UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
        )).context("shared template add_leaf")?;
    }
    let utreexo_root = commitment(&state).context("shared template commitment")?;
    let merkle_root = compute_root(coinbase_txid, &pt.merkle_path);

    // Re-wrap the stripped coinbase with the daemon's witness bytes for
    // submitblock (same helper the extended-share path uses).
    let full_coinbase = crate::wrap_stripped_with_segwit_witness(
        &coinbase_stripped, &pt.coinbase_witness_bytes, &pt.coinbase_suffix,
    );

    let wire = NewTemplateDinero {
        merkle_root,
        utreexo_root,
        ..pt.wire.clone()
    };

    Ok(SharedTemplate { wire, coinbase_full_hex: hex::encode(full_coinbase), outputs })
}
```

Notes for the implementer: `wrap_stripped_with_segwit_witness` is currently a private fn in `main.rs:1180` — make it `pub(crate)`. Check `NewTemplateDinero` derives `Clone` (it's used with struct-update syntax here); if not, construct field-by-field. Check the exact filter-input rule in dinerod (`src/consensus/filter_commitment.cpp` — whether zero-value/OP_RETURN outputs are included in the filter); mirror it exactly — the `filter(|o| o.value_una > 0)` line above is the expected rule but MUST be verified against the daemon source before committing, and the regtest E2E in Task 7 is the backstop.

- [ ] **Step 4: Run to verify pass** — `cargo test -p dinero-sv2-pool shared_template` → 1 passed.

- [ ] **Step 5: Commit** — `feat(pool): pool-owned shared template builder`

### Task 6: serve_miner wiring + CLI flags

**Files:**
- Modify: `crates/dinero-sv2-pool/src/main.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5.
- Produces: a pool where a miner that sends `SetRewardMode{mode:1, payout_script}` gets shared jobs and standard-share handling; solo miners are untouched.

- [ ] **Step 1: Add CLI flags** to the pool's arg struct (same clap pattern as `--share-leading-bits`):

```rust
/// PPLNS operator fee in basis points (200 = 2%).
#[arg(long, default_value_t = 200)]
shared_fee_bps: u32,
/// Max contributor outputs per shared block (fee output excluded).
#[arg(long, default_value_t = 20)]
shared_max_outputs: usize,
/// Minimum contributor output value in una; smaller slices carry forward.
#[arg(long, default_value_t = 10_000)]
shared_dust_una: u64,
/// PPLNS window journal path.
#[arg(long, default_value = "/var/lib/dinero-sv2/pplns-journal.jsonl")]
pplns_journal: std::path::PathBuf,
```

The fee script: derive from the existing `--payout-address` (the pool already resolves it to a script for GBT; reuse that resolved script).

- [ ] **Step 2: Shared state.** Create at startup (near the `Ledger`):

```rust
let window = Arc::new(Mutex::new(PplnsWindow::restore(
    WindowJournal::load(&args.pplns_journal)?, 14_400,
)));
let journal = Arc::new(Mutex::new(WindowJournal::open(&args.pplns_journal)?));
```

On each template refresh (where `new template` is logged), additionally build the shared variant:

```rust
let shared = {
    let w = window.lock().unwrap();
    let weights = w.weights();
    let p = split::SplitParams {
        reward_una: pt.coinbase_value_una,
        fee_bps: args.shared_fee_bps,
        fee_script: &fee_script,
        max_outputs: args.shared_max_outputs,
        dust_una: args.shared_dust_una,
        // No finder yet at template time — use the fee script so the
        // empty-window template still validates; the finder variant is
        // only needed at block-submit time and this template's split
        // already sums correctly.
        finder_script: &fee_script,
    };
    shared_template::build_shared_template(&pt, split::compute_split(&weights, &p))
};
```

Store `Option<SharedTemplate>` alongside the `PoolTemplate` handed to `serve_miner` (log + `None` on build error; shared miners then simply get no new job until the next refresh — never crash the pool).

- [ ] **Step 3: Per-connection mode state.** In `serve_miner`, add `let mut reward_mode: Option<Vec<u8>> = None;` (None = solo; `Some(payout_script)` = shared). Handle the new frame in the miner-message match (alongside `MSG_SUBMIT_SHARES_STANDARD` / `_EXTENDED`):

```rust
MSG_SET_REWARD_MODE => {
    match decode_set_reward_mode(&frame.payload) {
        Ok(m) if m.mode == 1 => {
            // Shape check: 34-byte taproot script (0x51 0x20 …).
            if m.payout_script.len() == 34
                && m.payout_script[0] == 0x51 && m.payout_script[1] == 0x20 {
                info!(channel_id, payout = %hex::encode(&m.payout_script), "miner switched to SHARED mode");
                reward_mode = Some(m.payout_script);
                // push the current shared job immediately:
                if let Some(st) = current_shared.as_ref() {
                    push_shared_job(&mut session, channel_id, st, &window).await?;
                }
            } else {
                send_share_error(&mut session, channel_id, 0, "bad-payout-script").await?;
            }
        }
        Ok(_) => { reward_mode = None; }  // explicit solo
        Err(e) => {
            warn!(error = %e, "bad SetRewardMode payload");
            send_share_error(&mut session, channel_id, 0, "bad-payload").await?;
        }
    }
}
```

- [ ] **Step 4: Shared job push.** New fn (next to `push_job`):

```rust
async fn push_shared_job(
    session: &mut NoiseSession<TcpStream>,
    channel_id: u32,
    st: &SharedTemplate,
    window: &Arc<Mutex<PplnsWindow>>,
    payout_script: &[u8],
) -> Result<()> {
    let snph = SetNewPrevHash {
        channel_id,
        prev_hash: st.wire.prev_block_hash,
        min_ntime: st.wire.timestamp,
        nbits: st.wire.difficulty,
    };
    session.write_frame(MSG_SET_NEW_PREV_HASH, &encode_set_new_prev_hash(&snph)).await?;
    session.write_frame(MSG_NEW_MINING_JOB, &encode_new_template(&st.wire)?).await?;
    let (bps, shares) = {
        let w = window.lock().unwrap();
        (w.miner_bps(payout_script), w.len() as u64)
    };
    let ws = WindowStatus { channel_id, window_bps: bps, window_shares: shares };
    session.write_frame(MSG_WINDOW_STATUS, &encode_window_status(&ws)?).await?;
    Ok(())
}
```

Wherever `serve_miner` currently pushes a fresh job on template change, branch: shared miners get `push_shared_job` (no `MSG_UTREEXO_STATE` / `MSG_COINBASE_CONTEXT` — those are JD-only), solo miners keep `push_job` exactly as today. (Adapt `encode_new_template` to whatever the existing solo push uses to serialize `NewTemplateDinero` for `MSG_NEW_MINING_JOB` — reuse the same function.)

- [ ] **Step 5: Standard-share handling for shared miners.** The existing `MSG_SUBMIT_SHARES_STANDARD` arm (main.rs:683) validates against the DAEMON template. For a shared-mode miner, validate against `current_shared.wire` instead; on acceptance:

```rust
// credit the PPLNS window + journal:
let weight = share_weight(&share_target);
let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
{
    let mut w = window.lock().unwrap();
    w.record(payout.clone(), weight, ts);
}
{
    let mut j = journal.lock().unwrap();
    let _ = j.append(&WindowEntry { payout_script: payout.clone(), weight, unix_ts: ts });
    if j.should_compact() {
        let w = window.lock().unwrap();
        let _ = j.compact(&w);
    }
}
```

On `meets_block`: submit via the existing `try_submit_block(&st.wire, &share, &st.coinbase_full_hex, &[], rpc)`; log `★ SHARED block ACCEPTED — split across N contributors` with the output list, and `ledger.credit_block` keyed by the payout script (switch `Ledger`'s key type usage at the call site: `MinerKey` is `[u8;32]` — for shared miners hash the payout script `Sha256(payout)` into the key so the existing ledger type is reusable without a refactor).

`SystemTime::now()` is fine here (pool binary, not a Workflow script).

- [ ] **Step 6: Run the full pool suite** — `cargo test -p dinero-sv2-pool` → all green; `cargo build --release -p dinero-sv2-pool` → clean.

- [ ] **Step 7: Commit** — `feat(pool): shared-mode registration, jobs, PPLNS crediting, block split`

### Task 7: Regtest E2E — two miners split a block

**Files:**
- Create: `crates/dinero-sv2-pool/tests/shared_split_e2e.rs` (`#[ignore]`d; needs a local regtest dinerod, same harness style as `tests/regression_bad_utreexo_root.rs` — read that file first and reuse its spawn/RPC helpers)

**Interfaces:** none downstream; this is the ship gate for Phase A.

- [ ] **Step 1: Write the test.** Skeleton (adapt helper names to what `regression_bad_utreexo_root.rs` actually provides):

```rust
//! Two shared miners → one block → coinbase pays both pro-rata + fee.
//! Run: cargo test -p dinero-sv2-pool --test shared_split_e2e -- --ignored --nocapture
#[test]
#[ignore]
fn shared_block_coinbase_pays_window_contributors() {
    let (dinerod, rpc) = spawn_regtest_dinerod();          // from existing harness
    let pool = spawn_pool_against(&rpc, &["--shared-fee-bps", "200"]);
    let miner_a = connect_shared_miner(&pool, payout_script(0xA1));
    let miner_b = connect_shared_miner(&pool, payout_script(0xB2));
    // Feed shares: A submits ~3x B's weight, then grind until a block
    // target share lands (regtest difficulty is trivial).
    let block_hash = mine_until_block(&[&miner_a, &miner_b]);
    let block = rpc.get_block(&block_hash);
    let coinbase_outputs = parse_value_outputs(&block.tx[0]);
    assert!(coinbase_outputs.iter().any(|o| o.script == payout_script(0xA1)));
    assert!(coinbase_outputs.iter().any(|o| o.script == payout_script(0xB2)));
    let a = value_for(&coinbase_outputs, payout_script(0xA1));
    let b = value_for(&coinbase_outputs, payout_script(0xB2));
    assert!(a > b, "A contributed more, must earn more");
    assert_eq!(coinbase_outputs.iter().map(|o| o.value).sum::<u64>(), block_reward());
}
```

The point of this test is the daemon accepting a **multi-output shared coinbase** (v2 roots + DNRW + all-scripts DNRF) — it is the backstop for the filter-rule verification flagged in Task 5. Regtest note: the utreexo maturity-leaf fork activates at height 20 on regtest — mine past height 20 first (`generatetoaddress 21`) so the shared blocks exercise the v2 path; the jd constant to pass in any regtest-side leaf computation is 20, not the mainnet 60_000.

- [ ] **Step 2: Run it** — `cargo test -p dinero-sv2-pool --test shared_split_e2e -- --ignored --nocapture` → PASS (iterate on Task 5/6 until it does; this is where any daemon-rule mismatch surfaces).

- [ ] **Step 3: Commit** — `test(pool): regtest E2E — shared block splits coinbase across contributors`

### Task 8: Phase A ship gate — deploy to SJ

- [ ] **Step 1:** `cargo test --workspace` green; push branch; open PR "feat: shared-reward PPLNS mining (pool)"; merge per repo convention (no CI — local suites are the gate).
- [ ] **Step 2:** Deploy: `rsync -az --exclude target --exclude .git ~/src/dinero-sv2/ root@173.249.200.59:/root/dinero-sv2/` (NO `--delete`), build on SJ (`cargo build --release -p dinero-sv2-pool`), `systemctl restart dinero-sv2-pool`, confirm `active` + `new template` lines within 30 s.
- [ ] **Step 3:** Live probe: run the existing `dinero-sv2-miner` (solo/extended) against the pool → shares still accepted (solo regression check). Existing phones (solo) must be unaffected — watch `journalctl -u dinero-sv2-pool` for 5 minutes.

---

## Phase B — phone FFI (`~/src/dinero-rust`), branch `feat/shared-reward-mode`

### Task 9: `dpi` client shared mode

**Files:**
- Modify: `dpi/src/sv2_mining.rs`
- Modify: `dpi/src/ffi.rs` (new create function; old one keeps SOLO semantics)

**Interfaces:**
- Consumes: Task 1 messages (path-dep on the dinero-sv2 workspace picks them up).
- Produces: `Sv2ClientConfig { …, reward_mode: RewardMode }` with `pub enum RewardMode { Solo, Shared }`; FFI `dpi_sv2_client_create2(host, port, server_pubkey_hex, payout_script_hex, reward_mode: u8, out_error) -> *mut DpiSv2Client` (0=solo, 1=shared). Job events gain `"mode":"solo"|"shared"` and `"window_bps":u32` fields.

- [ ] **Step 1: Failing test** (in `sv2_mining.rs` tests): shared-mode `handle_frame` on `MSG_NEW_MINING_JOB` must emit a job snapshot whose `utreexo_root`/`merkle_root` equal the pool template **verbatim** (no local recompute), and `MSG_WINDOW_STATUS` must emit a status event carrying `window_bps`:

```rust
#[test]
fn shared_mode_mines_pool_template_verbatim() {
    let mut state = RuntimeState { reward_mode: RewardMode::Shared, ..Default::default() };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tmpl = NewTemplateDinero { template_id: 5, merkle_root: [0x22; 32],
        utreexo_root: [0x33; 32], /* fill remaining fields as in existing tests */ };
    futures::executor::block_on(handle_frame(
        MSG_NEW_MINING_JOB, &encode_new_template(&tmpl).unwrap(), &mut state, &tx)).unwrap();
    let evt = rx.try_recv().unwrap();       // Sv2Event::Job
    // snapshot fields must be the pool's roots verbatim:
    match evt { Sv2Event::Job { job } => {
        assert_eq!(job.utreexo_root, hex::encode([0x33u8; 32]));
        assert_eq!(job.merkle_root, hex::encode([0x22u8; 32]));
        assert_eq!(job.protocol, "sv2-shared");
    }, _ => panic!("expected job") }
}
```

- [ ] **Step 2: Implement.** In `run_client`: after channel-open success, if `config.reward_mode == RewardMode::Shared`, send `MSG_SET_REWARD_MODE { channel_id, mode: 1, payout_script }`. In `handle_frame`: when `state.reward_mode == Shared`, `MSG_NEW_MINING_JOB` skips `try_emit_prepared_job`/`prepare_job` and emits the snapshot directly from the decoded template (protocol `"sv2-shared"`, `coinbase_txid` empty, `includes_dnrf_commitment: true`); `MSG_UTREEXO_STATE`/`MSG_COINBASE_CONTEXT` are ignored in shared mode. Handle `MSG_WINDOW_STATUS` → `Sv2Event::Status { state: "window", message: format!("{:.2}% of next shared block", bps as f64 / 100.0), … }` and stash `window_bps` on the next Job snapshots. Share submission in shared mode sends **`MSG_SUBMIT_SHARES_STANDARD`** (`SubmitSharesDinero { channel_id, sequence_number, job_id, nonce, timestamp, version }`) instead of extended.
- [ ] **Step 3:** FFI: add `dpi_sv2_client_create2` (same body as `dpi_sv2_client_create` + `reward_mode: u8` param mapped to the enum; old function passes `Solo`). Regenerate/extend `include/dpi_ffi.h` with the new prototype.
- [ ] **Step 4:** `cargo test -p dpi sv2` → green (both old solo tests + new shared test). Extend `dpi/examples/sv2_session_probe.rs` with `SV2_REWARD_MODE=shared` env support; run against the live SJ pool once Phase A is deployed → session stable, `window` status events received.
- [ ] **Step 5: Commit** — `feat(sv2): shared reward mode in the mining client + create2 FFI`

### Task 10: xcframework rebuild + embed

- [ ] **Step 1:** `dpi/build-ios.sh` (stub-parity build, same as 2026-07-09); verify `strings target/DPI.xcframework/ios-arm64/libdpi.a | grep -c dpi_sv2_client_create2` = 1.
- [ ] **Step 2:** In a DineroDPI worktree off `origin/main`: replace `DineroDPI/Libraries/DPI.xcframework`, sync `DineroDPI/Libraries/Headers/dpi_ffi.h`; commit `contribute: embed DPI.xcframework with shared reward mode`.

---

## Phase C — DineroDPI UI, branch continues from Task 10's worktree

### Task 11: Reward-mode UI

**Files:**
- Modify: `DineroDPI/DineroDPI/Core/Mining/ContributeViewModel.swift`
- Modify: `DineroDPI/DineroDPI/Core/Mining/SV2Client.swift`
- Modify: the Contribute screen SwiftUI view (grep `Pool Endpoint` for the file)

- [ ] **Step 1:** `ContributeViewModel`: add `@Published var rewardMode: RewardMode` backed by UserDefaults key `contribute.rewardMode` (raw `"shared"`/`"solo"`, **default `.shared`** when unset), plus `@Published var windowBps: Int?`. `RewardMode` is a two-case enum in the same file.
- [ ] **Step 2:** `SV2Client`: call `dpi_sv2_client_create2(…, mode)` (mode from the view model); surface `window` status events → `windowBps`. Job snapshot's `protocol == "sv2-shared"` drives a "Shared" badge.
- [ ] **Step 3:** Contribute view: segmented "Reward mode" control above Start Mining — **Shared** ("steady split of every block the pool finds") / **Solo** ("whole block or nothing, mined to your address"); status line shows `~X.XX% of next shared block` when `windowBps != nil`.
- [ ] **Step 4:** Build for simulator (`xcodebuild -project DineroDPI/DineroDPI.xcodeproj -scheme DineroDPI -destination "generic/platform=iOS Simulator" build`) → BUILD SUCCEEDED. Toggle both modes in the sim against the live pool; confirm shared mode shows the window line and solo mode still mines extended shares (pool logs).
- [ ] **Step 5:** Commit; PR "contribute: shared reward mode (PPLNS)"; merge per convention.

### Task 12: Rollout verification (ship gate)

- [ ] **Step 1:** Phone on the new build mines SHARED for 10+ minutes: pool logs show `SHARED` registration, standard shares credited, `WindowStatus` pushes; UI shows the window percentage.
- [ ] **Step 2:** Flip the phone to SOLO: extended shares flow exactly as tonight (regression).
- [ ] **Step 3:** When the first shared block lands on mainnet: fetch the block, verify the coinbase output set matches the pool's logged split (values + scripts + 2% fee) before announcing. Update `MemoryMD/dinero-sv2.md` with the shared-mode deployment facts.

---

## Self-review notes (done at write time)

- Spec coverage: decisions 1–4 → Tasks 4/6 (fee, split), 2 (PPLNS), 11 (default shared), 1/6 (opt-in wire). Components 1–6 → Tasks 1, 2/3, 5, 6, 9, 11. Error handling → Tasks 5 (builder errors), 6 (bad-payout-script, solo fallback), 3 (corrupt journal). Testing section → Tasks 1–5 units, 7 regtest E2E, 9 probe, 12 rollout.
- Open verification flagged inline (not a placeholder — a required check): the DNRF filter-input rule (which outputs enter the block filter) must be confirmed against `dinero-v8/src/consensus/filter_commitment.cpp` in Task 5, with Task 7's regtest E2E as the backstop.
- Type consistency: `CoinbaseOutput{value_una, script_pubkey}` used uniformly; `WindowEntry` shared by Tasks 2/3/6; `SharedTemplate` produced in 5, consumed in 6/7.
