# DNRS mining compatibility

Mainnet DNRS activates at height 111000. This change fixes pool-owned shared
coinbases and miner-owned solo/JD coinbases. Direct node-RPC miners using
`mining.getjob`/`mining.submit` leave assembly with the upgraded daemon.

The pool copies the daemon's exact post-block shielded root. Shared payout
changes affect transparent coinbase outputs and their Utreexo/filter roots;
they do not authorize recomputing or omitting DNRS. Missing, malformed,
duplicate or nonzero-valued daemon commitments are refused. Extended shares
must include exactly the same canonical zero-valued DNRS before crediting or
submission. The pool no longer drops a template's transactions after Utreexo
pre-state derivation fails: its coinbase commits to the original transaction
set, so that fallback could invalidate DNRS, witness commitment and fee totals.

CoinbaseContext has an optional, exactly 32-byte trailing root extension.
Absent extension retains the old wire encoding. A present extension contains
only the root; clients construct the canonical `6a25 DNRS 01 <32 bytes>` script.
Updated decoders reject truncated or oversized extensions. Older strict
decoders reject the extension instead of silently constructing invalid work.
CPU and GPU SV2 solo clients therefore require the 0.2.9 binaries from this
change (pool 0.1.4). Record source commit and SHA-256 as well as version. Shared miners
grind pool-owned templates, but distributions should update all sidecars together.
No new hardware kernel or hashing algorithm is required.

Pool option `--state-commitment-height` defaults to mainnet 111000. Set it to
1 for default regtest, 4294967295 for dormant testnet, or the exact test override.
The source checks actual commitment presence as well as the configured height;
never point an activated pool at an old daemon or lower this guard to bypass a
missing commitment. Existing jobs may become stale while a backend is refused.

## Verification

Run `cargo test --workspace`. Then build the CPU miner and, where actual GPU
hardware is available, the GPU miner. With an activation-capable dinerod:

```sh
cargo build -p dinero-sv2-miner -p dinero-sv2-gpu-miner
export DINEROD_BIN=/absolute/path/to/dinerod
export DINEROMINER_BIN="$PWD/target/debug/dinero-sv2-miner"
export DINEROGPUMINER_BIN="$PWD/target/debug/dinero-sv2-gpu-miner"
cargo test -p dinero-sv2-pool --test shared_split_e2e \
  shared_block_coinbase_pays_window_contributors -- --ignored --nocapture
cargo test -p dinero-sv2-pool --test shared_split_e2e \
  solo_miner_preserves_dnrs_through_pool -- --ignored --nocapture
cargo test -p dinero-sv2-pool --test shared_split_e2e \
  solo_gpu_miner_preserves_dnrs_through_pool -- --ignored --nocapture
```

Run these serially: CPU/GPU solo fixtures share ports. Shared testing crosses
height 26; real CPU/GPU solo testing crosses height 2, before regtest's Utreexo
maturity-leaf boundary (the reference solo clients currently use mainnet's
maturity height). These are accelerated DNRS boundaries, not claims of mining
production height 111000. The shared fixture also checks payout weights, fees,
full reward accounting and exact commitment preservation in an accepted block.

Recorded local evidence: shared block at height 26 accepted, CPU solo block at
height 2 accepted, Metal GPU solo block at height 2 accepted. No CUDA/OpenCL
runtime qualification is implied by the Metal result. Negative unit cases cover
missing, extra, duplicate, altered, malformed and nonzero-valued commitments.

## Deployment

Before replacing the live pool: retain executable hash, unit configuration,
Noise key, payout configuration and PPLNS journal; build a source-pinned candidate;
repeat the real process tests on Linux; record candidate binary SHA-256. Preserve
pool identity and accounting across restart. Upgrade the daemon before mainnet
activation and distribute new SV2 CPU/GPU clients. Replace bundled GUI sidecars
only with matching release artifacts and refreshed hash locks.

The previously observed SJ executable SHA-256 was
`fdc743c73490774738fc0409b23d400a4ea443d1b46aaafba91ab618fe04835e`.
Its deployed main.rs matched historical commit
`55567f203a289f0192ccd1a8e2cc2e2046038de7`, but that file match does not establish
complete binary build provenance. Do not label the old binary activation-ready.


The live SJ source snapshot also contains older CPU/GPU clients that submit
job ID zero; the current pool requires the actual job ID. Client rollout must
precede the pool swap. Do not weaken stale-share rejection to accommodate old
clients. A local executable named `~/.local/bin/dinero-miner` may actually be an
SV2 client: inspect `--help`/`--version`, not only the filename. Direct core RPC
CPU/Metal GPU binaries were separately verified mining enforced DNRS blocks and
retaining identical tip/shielded state after daemon restart.
