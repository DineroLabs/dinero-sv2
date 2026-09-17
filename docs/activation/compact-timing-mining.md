# Compact proofs and 60-second mining preparation

Qualification candidate, not a deployed release. Pair this source with daemon
commit `8f6609e829d5d0895debf4db4cb4b07ebf459172` (Dinero v8 PR #767).
That daemon combines the compact format/vector/sanitizer stack with the dormant
60-second ASERT and 0.5 DIN tail rules. Production activation remains disabled.

## Components

- Pool candidate 0.1.9 retains the daemon's exact selected transaction bytes,
  rebuilds payout-related Utreexo/filter commitments and preserves DNRS/DNRW.
  The existing daemon-assisted exclusion path remains required for bad proofs.
- CPU/GPU candidate 0.2.13 fixes job lifecycle behavior. CPU now advances the
  work generation on `SetNewPrevHash`; GPU restarts a valid shared job after a
  target-only update and forgets that job on a new prev-hash.
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
transactions through a real pool across timing activation. They check exact
persisted bytes, selected transactions, DNRS, DNRW over unchanged bytes, full
DNRF including spent scripts, and the 100 DIN subsidy plus selected fees. The
daemon's acceptance enforces Utreexo roots and activated commitments. Bulk
setup mining has its own 300-second-per-batch timeout; steady-state pool/wallet
deadlines and commitment checks are unchanged. Tests retain process logs.

`compact_timing_pool_proof_recovery` uses a shorter chain and injected RPC proof
failure to require a daemon-rebuilt transaction set and changed DNRS while the
valid compact unshield still confirms. Mandatory-height DNRW is covered by the
longer tests, not by this short recovery fixture.

The existing shared PPLNS split/pool-fee and ordinary proof-recovery cases stay
registered and execute in the separate DNRS compatibility workflow. Local
workspace baseline: 440 tests passed before the final fixture additions;
ignored real-process tests require their explicit commands above. Consult the
PR's current checks and retained logs for paired-run results, rather than
interpreting successful compilation or ignored tests as qualification.

## Release completion

Record source commits, binary hashes and actual backend/device identity.
Qualify Linux and ARM artifacts, physical Metal/CUDA/OpenCL workers as supported,
reconnect/failover and sustained job-switching/stale rates under real PoW load.
Refresh packaged wallet/GUI sidecars and their hash locks together. Preserve
Noise identity, payout configuration and the PPLNS journal during pool rollout.
The daemon, pool and workers have independent versions; v8.1.13's release
manifest must bind them explicitly. No tag or production replacement is made
by these tests.
