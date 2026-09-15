# Pool proof recovery: Linux qualification

## Scope and source identity

Qualification uses the companion daemon commit
`b53bc958b3360ca9b8f52aa0488714fec51e68f0` and pool commit
`de668bbd3a21430cb79a3a8d96aa29d02c1f93e7` (including `f4341db`).
The source snapshots include pinned submodule contents. Production sources are
unchanged; the test-readiness follow-up described below is overlaid for testing.
The daemon's shallow Git metadata preserves its canonical repository and commit
identity in the binary.

Local qualification uses an isolated Ubuntu 24.04 **x86_64 container under emulation**
on macOS arm64. Compiler: GCC 13; Rust: 1.91.1; OpenSSL: pinned static 3.5.7.
GPU mining is disabled to match the fleet's headless release configuration.
This validates Linux x86_64 compilation and behavior, not native-host performance.
It does not exercise production chainstate, deployment, or live miner reconnects.

Evidence directory in the pool worktree:
`target/linux-qualification-20260915/`. It contains the source manifest, archive
checksum, container identity, tool versions, build/test logs and candidate binary
checksums. The build ran on copies inside the container and did not touch
running services. The task-owned container was removed after qualification;
the source snapshots, logs and candidate binaries remain in the evidence directory.

## Required checks

- Build the daemon and C++ template-selection tests.
- Run daemon exclusion, DNRS acceptance, coordinator mining and restart checks.
- Run the Rust workspace tests.
- Exercise real pool mixed shield/unshield inclusion, injected proof-failure
  recovery, shared contributor payouts and CPU solo DNRS compatibility with
  the optimized pool and miner binaries.
- Record checksums and dynamic dependencies of the resulting candidate binaries.

## Rollout prerequisites

1. Development branches were published daemon-first so the pinned CI checkout
   resolves. Native Linux CI passed at pool commit
   `8119b9bae9a798286f1af4ef9618b35318d7e384`. Its differences from the
   locally snapshotted pool commit are workflow configuration and a test-only
   daemon-readiness fix. The same readiness helper is overlaid into the local
   test snapshot and recorded in `test-harness-overlay.json`.
   The native result and retained logs are recorded below.
2. Select release versions and update the pool crate version and lockfile to
   match its release tag. These qualification candidates retain the existing
   package versions; identify them by source commit and checksum.
3. Build/release through the normal pipelines. Validate the exact release
   artifacts and preserve the existing SJ configuration, payout settings and
   rollback binaries.
4. With owner approval, qualify the daemon first, then enable the pool recovery
   build. Check fresh work for each enabled mining mode, real accepted shares,
   backend tip agreement and confirmation behavior. Nonempty JD remains gated.
5. Record the deployed checksums and observe recovery/availability after rollout.

No production action is authorized by this document. A retained mempool
transaction may still fail later consensus/anchor rules; this patch does not
promise eventual confirmation of every excluded transaction.

## Findings during qualification

The initial default daemon configuration linked `libOpenCL.so.1`. The fleet
release configuration explicitly disables GPU mining, so local qualification and
the native CI workflow were corrected to use `-DENABLE_GPU_MINING=OFF` and check
the direct ELF dependencies against the release allow-list. This was a build
configuration change; the transaction recovery implementation did not change.

An initial parallel run under emulation failed the existing backend-reconciliation
test's 50 ms timeout: the fake RPC server received EOF before any request. The
four-test backend suite passed when rerun sequentially (0.23 seconds). The final
local full-suite run is sequential and the native CI run uses normal test
concurrency. The initial failure log is retained; no test or production timeout
was changed and no test was removed.

The optimized shared-payout test also exposed a startup race in its existing
fixture: the daemon writes its cookie before starting the HTTP listener. Commit
`8119b9b` changes the test helper to await a successful `getblockcount` response
before issuing wallet calls, preserving its existing 15-second startup deadline.
The failing payout case then passed. This changes no production behavior.

## Local result

All final checks passed on the headless build:

- Rust workspace: **438 passed, 0 failed, 9 opt-in ignored**, with sequential
  execution after compilation completed.
- Daemon CTest entries: `BlockTemplateDeterminism` and `MiningTemplateExclusions`
  passed, including accepted filtered and all-excluded DNRS blocks.
- Default daemon GBT/coordinator mining, DNRS restart persistence and dormant
  controls passed.
- Optimized pool/miner process tests: mixed shield/unshield inclusion,
  per-transaction proof-failure recovery, shared payouts and CPU solo DNRS
  compatibility all passed.
- Direct daemon ELF dependencies match the fleet release allow-list. The pool
  requires `libgcc_s`, `libm`, `libc` and the runtime loader.

Candidate checksums (these are qualification artifacts, not published releases):

```text
4547b67194ea4499250221166a49437b85d16c37f8e2f011351865b830e34114  dinerod-linux-x86_64
3d6a9c620e63b4788fb3c305197a823d707821a32973a8ad23043037dd2d0549  dinero-sv2-pool-linux-x86_64
```

Structured local evidence is in `qualification-result.json`. The production
source is unchanged from the original recovery commits; later commits change
qualification configuration and test readiness only.

## SJ compatibility preflight (read-only)

SJ reports x86_64 Ubuntu 24.04.4, glibc 2.39, `libstdc++6` 14.2.0 and `libudev1`
255.4. The required `GLIBCXX_3.4.32` symbol is present. The candidate daemon's
maximum imported glibc symbol version is 2.38, so the observed host libraries meet
these ABI requirements. This is a library/architecture check, not a live candidate
startup or production-chainstate test. Only OS/package/library queries were run;
no service, wallet, mempool, binary or configuration was changed on SJ.

## Native Ubuntu result

[Run 34966435010](https://github.com/DineroLabs/dinero-sv2/actions/runs/34966435010)
passed on commit `8119b9bae9a798286f1af4ef9618b35318d7e384`, including the
test-readiness fix:

- Rust workspace with normal test concurrency: **438 passed, 0 failed, 9 ignored**.
- Native daemon CTest: **2 entries passed**, including request-local exclusions.
- Fleet ELF dependency gate passed.
- All **4 optimized process tests passed**: mixed shield/unshield inclusion,
  injected proof-failure recovery, shared payouts and CPU solo DNRS compatibility.

The full log and run metadata are retained as `native-ci-8119b9b.log` and
`native-ci-8119b9b.json` in the evidence directory. The earlier native run
[34965255415](https://github.com/DineroLabs/dinero-sv2/actions/runs/34965255415)
also passed at `0c75510`, with identical production recovery code.

Linux qualification is complete. Release preparation, artifact validation and
owner-approved deployment remain; no release tag or production change was made.
