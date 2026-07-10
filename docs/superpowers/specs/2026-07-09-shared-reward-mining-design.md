# Shared-Reward Mining (PPLNS) — Design

**Date:** 2026-07-09
**Repos touched:** `dinero-sv2` (pool — primary), `dinero-rust` (`dpi` phone FFI), `DineroDPI` (Contribute UI)
**Status:** approved design, pre-implementation

## Problem

Contribute mining is all-or-nothing today: every phone mines a miner-owned
coinbase paying only itself (extended/JD shares). At phone-class hashrate
(~40 kH/s) versus a MH/s-class network, a solo block can take days to weeks —
most participants earn nothing and churn out. We want an opt-in-per-phone
**shared mode** where many phones mine together and split each found block.

## Decisions (settled with the owner)

1. **Custody model: trustless coinbase split.** The winning block's coinbase
   itself pays every eligible contributor. The pool never holds funds — no
   hot wallet, no payout transactions, no balances.
2. **Split scheme: PPLNS** (pay-per-last-N-shares), difficulty-weighted.
3. **Default mode: Shared.** Phones that never touch the setting mine shared;
   Solo (today's behavior, unchanged) is a per-phone opt-out.
4. **Operator fee: 2% from day one**, taken off the top of shared blocks to
   the fleet payout address. Configurable (`--shared-fee-bps`, default 200);
   changing it is config, not code.

## Architecture: pool-owned jobs for shared mode

Chosen over "JD with pool-dictated outputs" (rejected: constant output-set
churn forces every phone to re-derive merkle+utreexo roots on every window
update, and the pool must byte-validate the result anyway) and over custodial
balances (rejected by decision 1).

- **Shared mode:** the pool assembles the entire coinbase — PPLNS split
  outputs + fee output + DNRW witness commitment + DNRF filter commitment —
  computes the merkle root and the v2 utreexo root itself (primitives all
  exist in `dinero-sv2-jd` as of the 2026-07-09 fixes), and pushes a
  ready-to-grind job. The phone hashes the 128-byte header and submits
  **standard** shares (`SubmitSharesDinero`: nonce/ntime/version — the
  pre-JD path already in the wire protocol).
- **Solo mode:** today's extended-share path, byte-for-byte unchanged
  (miner-owned coinbase, DNRW+DNRF guards, v2-leaf recompute on the phone).

Trust note: shared miners trust the pool to build the split honestly, the
same way they already trust it for work selection. Every found block's
coinbase is publicly auditable on-chain against the advertised rules;
skeptics keep Solo mode.

## Components

### 1. Pool: miner identity & mode registration

- A shared-mode phone declares `mode=shared` plus its **payout script** at
  channel open. Wire: new extension message `MSG_SET_REWARD_MODE`
  (channel_id, mode: u8 {0=solo,1=shared}, payout_script: bytes) sent
  between `OpenStandardMiningChannel.Success` and the first share. A miner
  that never sends it is Solo (backward compatible with existing clients).
- **Ledger identity becomes the payout script** (replaces the Noise-static
  key, which bucketed all anonymous phones under `[0u8; 32]`). Solo miners
  keep being tracked by their extended-share payout script for stats.

### 2. Pool: PPLNS window (`accounting.rs` rework)

- Rolling window of the last **N accepted shares** across all shared miners.
  Each entry: `{payout_script, weight, timestamp}` where
  `weight = difficulty of the share's target at acceptance` (vardiff-fair:
  one hard share counts as much as many easy ones).
- **N is dynamic:** sized so the window covers ~4 hours of pool-wide shared
  work (recomputed from observed share rate; floor 500 entries, cap 50_000).
- **Persistence:** append-only JSONL journal on disk
  (`/var/lib/dinero-sv2/pplns-journal.jsonl`), compacted to the live window
  on rotation; loaded on pool start. Survives restarts; losing it degrades
  gracefully (window rebuilds from new shares).

### 3. Pool: shared template builder

On every template refresh (and immediately on window change is NOT needed —
the split snapshot is taken per template, every ~16 s):

1. Snapshot the window → per-contributor weight sums.
2. `fee = reward * shared_fee_bps / 10_000` → fleet payout address.
3. Remainder pro-rata by weight, **top 20 contributors** by weight
   (`--shared-max-outputs`, default 20).
4. Drop outputs `< dust_floor` (`--shared-dust-una`, default 10_000 una);
   dropped and over-cap contributors keep their shares in the window —
   their **credit** carries forward to future blocks (no funds held).
5. Rounding remainder (una) goes to the block finder's output if they're in
   the split, else to the fee output.
6. Assemble coinbase: `[split outputs…, fee output, DNRW, DNRF]`; compute
   merkle root; compute post-coinbase v2 utreexo root
   (`leaf_hash_for_height`, `is_coinbase=true`); build the standard job.

Invariant: `sum(outputs) == coinbase_value_una` (the existing pool guard
already enforces this for extended shares; the builder asserts it).

### 4. Pool: standard-share validation & block path

- Standard shares from shared miners validate against the pool-owned
  template (`HeaderAssembly::hash(pool_template, share)`), credited to the
  window with their vardiff weight.
- A block-target standard share submits the pool-assembled block (same
  `assemble_block_hex` path; coinbase witness bytes are the pool's own).
- `ledger.credit_block` records the finder's payout script for stats.
- With each job push to a shared miner, the pool includes that miner's
  current window fraction (basis points) in a small extension message
  `MSG_WINDOW_STATUS` (channel_id, window_bps, window_shares) — the source
  of the UI's "~3.1% of next shared block" line.

### 5. Phone (`dinero-rust` dpi FFI)

- `Sv2ClientConfig` gains `reward_mode: RewardMode` (`Solo | Shared`).
- Shared mode: after channel open, send `MSG_SET_REWARD_MODE` with the
  wallet payout script; skip `prepare_job` coinbase assembly entirely —
  mine `NewMiningJob`'s header fields verbatim; submit standard shares.
  (Less phone CPU/battery than solo mode, no root recompute.)
- Job snapshot exposes `mode`, and — from a new pool status field in the
  job push — the phone's current window percentage for UI display.
- Solo mode code path untouched.

### 6. DineroDPI Contribute UI

- New "Reward mode" control: **Shared** (default; copy: "steady split of
  every block the pool finds") / **Solo** ("whole block or nothing, mined
  to your address"). Backed by UserDefaults; default Shared for installs
  that never set it.
- Status shows mode + estimated window share ("~3.1% of next shared block").
- Payout address handling unchanged (wallet's Taproot receive address or
  the manual override).

## Error handling

- Shared miner sends no `MSG_SET_REWARD_MODE` → treated as Solo (old
  clients keep working).
- `MSG_SET_REWARD_MODE` with a malformed payout script → error message
  `bad-payout-script`, channel stays open, miner treated as Solo.
- Empty window at block find (first shared block ever) → whole reward minus
  fee to the finder.
- Pool restart mid-window → journal reload; if journal corrupt, start an
  empty window and log loudly (funds are never at risk — only unpaid
  *credit* is).
- Solo miners are never included in shared splits, and shared miners never
  receive from solo blocks.

## Testing

1. **Unit (pool):** split math — weights, 2% fee, dust carry-forward,
   top-20 cap, rounding invariant `sum(outputs)==reward`, empty-window
   fallback; window sizing/rotation; journal round-trip.
2. **Unit (jd/phone):** standard-share mining path (header-verbatim jobs);
   mode registration encode/decode.
3. **Regtest E2E:** two simulated miners with distinct payout scripts mine
   shared; assert a found block's coinbase pays both pro-rata + fee output,
   and dinerod accepts the block (v2 root + DNRW correct with >3 outputs).
4. **Live probe:** extend `dpi/examples/sv2_session_probe.rs` with shared
   mode against the SJ pool (session stability + window registration).
5. Every consensus-sensitive artifact (coinbase with many outputs) pinned
   the way tonight's fixes were: against a live daemon vector.

## Rollout

1. Pool ships first (backward compatible — old phones keep soloing).
2. dinero-rust FFI + xcframework rebuild.
3. DineroDPI UI + embed; default flips to Shared on update.
4. Monitor first shared block's coinbase on-chain against the ledger
   snapshot before announcing.

## Out of scope (explicitly)

- Custodial balances / payout transactions (rejected).
- Cross-pool share portability, sharechains (P2Pool) — revisit only if the
  fleet pool decentralizes.
- Mempool-tx-bearing shared templates: shared jobs are coinbase-only until
  the JD mempool story (parked since April) resolves; the standard-share
  path makes this trivial to lift later since the pool owns the template.
