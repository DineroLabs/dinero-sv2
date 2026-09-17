# Compact proofs and 60-second mining preparation

Qualification candidate, not a deployed release. Pair this source with daemon
commit `6714004b985e601d296ef1f5fe7d7440df0142aa` (Dinero v8 PR #767).
That daemon combines the compact format/vector/sanitizer stack with the dormant
60-second ASERT and 0.5 DIN tail rules. Production activation remains disabled.

## Components

- Pool candidate 0.1.9 retains the daemon's exact selected transaction bytes,
  rebuilds payout-related Utreexo/filter commitments and preserves DNRS/DNRW.
  The existing daemon-assisted exclusion path remains required for bad proofs.
- CPU/GPU candidate 0.2.13 fixes job lifecycle behavior. CPU now advances the
  work generation on `SetNewPrevHash`; GPU restarts a valid shared job after a
  target-only update and forgets that job on a new prev-hash. Both workers stop
  hashing at `--max-blocks` but wait for acknowledgement of the final share
  before closing the transport (bounded to 30 seconds). Immediate close had
  caused the pool's acknowledgement write to fail before block submission.
- Existing hashing kernels and the 128-byte SHA-256d header remain unchanged.
  The daemon supplies the network target; workers do not calculate ASERT.
- Transaction-bearing solo/JD work remains gated. Its separate protocol design
  is in [nonempty-job-declaration.md](nonempty-job-declaration.md).

## Test entry points

Build the actual binaries, then set absolute paths:

```sh
cargo build --locked --release -p dinero-sv2-pool -p dinero-sv2-miner -p dinero-sv2-gpu-miner
export DINEROD_BIN=/absolute/path/to/combined/dinerod
export DINEROPOOL_BIN="$PWD/target/release/dinero-sv2-pool"
export DINEROMINER_BIN="$PWD/target/release/dinero-sv2-miner"
export DINEROGPUMINER_BIN="$PWD/target/release/dinero-sv2-gpu-miner"
cargo test --locked --workspace
cargo test --locked -p dinero-sv2-pool --test worker_job_lifecycle -- --ignored --nocapture
cargo test --locked -p dinero-sv2-pool --test shared_split_e2e compact_timing_ -- --ignored --nocapture
```

The GPU entries require a physical supported GPU; select individual CPU tests
on a host without one. The Linux workflow explicitly runs CPU job lifecycle,
paired pool and actual CPU mining, and compact proof-failure recovery. It makes
no GPU runtime claim.

`worker_job_lifecycle` connects actual workers to a controlled Noise pool. It
checks independently hashed shares, timestamps above u32, immediate stale-work
cancellation while the next job is withheld, no revival by a target update,
and resumed hashing after a target-only update. The original CPU and Metal GPU
binaries each failed their corresponding regression. Both fixed workers passed
on physical macOS arm64 hardware, including the versioned 0.2.13 binaries.

The three `compact_timing_*mining/worker` cases create full regtest histories up
to the real mandatory DNRW height (10,670), then mine compact shield and unshield
transactions in successive blocks through a real pool across timing activation. They check exact
persisted bytes, selected transactions, DNRS, DNRW over unchanged bytes, full
DNRF including spent scripts, and the 100 DIN subsidy plus selected fees. The
daemon's acceptance enforces Utreexo roots and activated commitments. Bulk
setup mining has its own 300-second-per-batch timeout; steady-state pool/wallet
deadlines and commitment checks are unchanged. Tests retain process logs. For
local iteration, `DINERO_MINING_FIXTURE` can name an explicitly stopped,
disposable preactivation test datadir. Each case copies it into a fresh datadir,
requires an empty mempool and a height no greater than 10,670, then mines the
remaining history. The source is never opened as a database or modified. CI
does not set this option and starts from genesis.

`compact_timing_pool_proof_recovery` uses a shorter chain and injected RPC proof
failure to require exclusion of the compact shield and an accepted empty
replacement block with rebuilt DNRS; the excluded transaction stays in the
mempool. Mandatory-height DNRW is covered by the
longer tests, not by this short recovery fixture.

The existing shared PPLNS split/pool-fee and ordinary proof-recovery cases stay
registered and execute in the separate DNRS compatibility workflow. Local
workspace baseline: 440 tests passed before the final fixture additions;
ignored real-process tests require their explicit commands above. Consult the
PR's current checks and retained logs for paired-run results, rather than
interpreting successful compilation or ignored tests as qualification.

## Local results, 2026-09-17

- Actual CPU and physical Metal (Apple M4 Max) worker lifecycle: 2/2 passed.
- Paired compact pool, actual CPU, actual Metal GPU: 3/3 passed. Each mined
  shield at 10,671 and unshield at 10,672. Local iteration copied a stopped
  disposable history at 10,669; the candidate itself mined 10,670 onward.
- Compact injected-proof recovery: passed; excluded shield remains in mempool,
  empty recovery block accepted with rebuilt DNRS. Only the explicit
  `shielded_state_busy` response gets bounded retry during the final root query;
  timeout/other errors still fail, and the pool keeps running during the check.
- Red runs reproduced the CPU stale-generation bug, GPU target-update stall,
  both workers' premature final-share close, daemon chainstate/mempool lock
  inversion, and missing DNRW in daemon `generatetoaddress` at 10,670. The paired
  daemon pin includes both daemon repairs. Validation checks were not relaxed.

These are local regtest compatibility results, not Linux/GPU-family or live
network qualification. CI starts from genesis with the pinned daemon source.

## Throughput finding

The daemon's two-second template-selection budget returned a valid partial
template containing one of two independent compact transactions under local
load. Compatibility therefore tests each shape in a separate accepted block;
it does not require every pending transaction to fit one template. The original
ordinary mixed-transaction regression remains intact. Compact mixed/package
throughput under load remains a release measurement gate. The byte reduction
alone does not prove a higher realized transaction rate, and this work does not
raise the selection budget or skip proof validation to obtain a passing test.

## Release completion

Record source commits, binary hashes and actual backend/device identity.
Qualify Linux and ARM artifacts, physical Metal/CUDA/OpenCL workers as supported,
reconnect/failover and sustained job-switching/stale rates under real PoW load.
Refresh packaged wallet/GUI sidecars and their hash locks together. Preserve
Noise identity, payout configuration and the PPLNS journal during pool rollout.
The daemon, pool and workers have independent versions; v8.1.13's release
manifest must bind them explicitly. No tag or production replacement is made
by these tests.
