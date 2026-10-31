# Orchard mining compatibility

At Orchard activation, Core owns typed transaction selection, coinbase
finalization and mandatory proof framing. Pool 0.1.10 advertises the capable GBT
contract and requests immutable base and per-channel jobs from Core. It retains
the sealed frame and changes only the nonce and allowed timestamp at submission.
Normal backend submit fanout receives the complete block, never another
process's job identifier. Historical assembly and JD remain below activation.

Shared miners keep their existing wire protocol. PPLNS outputs are finalized
before hashing and each channel has a distinct coinbase. Rebuilding failure
cannot credit work on a replaced parent or backend generation.

Solo CPU and GPU miners 0.2.14 negotiate finalized coinbase support and send
their chosen payout script. They verify the one exact payout, height,
commitments and coinbase inclusion in the advertised header before starting
work. Solo shares never enter the shared PPLNS journal. Older solo clients get
an explicit upgrade refusal at Orchard activation, rather than an altered payout.

## Reproducible qualification

`Orchard immutable mining` builds the pinned Core backend and the process-local
coupled Orchard fixture at H102. Its prefix uses actual historical proof of work;
blocks use normal validation and storage. This fixture is never a shipping daemon
or a mainnet activation claim. The existing historical DNRS and compact-timing
workflows remain independent coverage.

Run the ignored shared test with `DINERO_ORCHARD_DAEMON_BIN`,
`DINERO_ORCHARD_CORE_SOURCE`, and `DINEROPOOL_BIN` set to explicit source/binaries:

```
cargo test --locked -p dinero-sv2-pool --test shared_split_e2e \
  shared_pool_crosses_orchard_with_exact_contributor_payouts -- --exact --ignored --nocapture
```

Run the actual solo miner with:

```
python3 -B crates/dinero-sv2-pool/tests/orchard_solo_e2e.py \
  --core-source /path/to/core --daemon /path/to/test_orchard_coupled_cycle_daemon \
  --pool /path/to/dinero-sv2-pool --miner /path/to/dinero-sv2-miner
```

For a real GPU use the GPU miner and `--gpu-backend metal` (or supported OpenCL/
CUDA hardware). A successful local Metal check does not establish Windows/Linux
GPU hardware qualification. Tests use generated isolated wallets and ports and
print source executable hashes; they do not connect to a production pool.
