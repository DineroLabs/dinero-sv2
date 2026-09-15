# Paired recovery release review

## Candidate scope

- Pool **0.1.9**, source `f16d71ca8312130d61ab94b52ea6c44e869fbab0`.
- Daemon **8.1.13**, source `06a7ff369569c3a3d95e446578aed737d256ee6e`.
- Candidate builds use the existing release workflows without creating tags or
  publishing GitHub releases. SJ deployment is a separate owner decision.

The daemon delta from the previously qualified `b53bc958b` is its CMake version.
The pool adds candidate packaging, an explicit packaged-executable override in
the process-test fixture, and the spent-input filter correction below.

This review covers the paired recovery changes relative to daemon `7a356337b`
and pool `a45b7ae`, plus candidate preparation. Daemon 8.1.13 also contains the
already merged changes since v8.1.12: P2P stale-tip recovery and probe hardening,
valid-fork reporting, exact invoice amounts, release symbols, and wallet/UI
fixes. This is not an independent re-review of all those earlier merges or a
qualification of the other platform release artifacts.

## Review result

The pool obtains complete bounded input proofs, checks identity and forest
membership, skips shielded-only and ephemeral inputs, and keeps transaction
bytes with each shared job. Recovery has bounded attempts and a caller timeout.
It rejects a changed parent or an old daemon that retains excluded transactions.

The daemon parses bounded exact transaction IDs and removes their transparent
descendant closure before commitment prediction and poison quarantine. It
recalculates fees and uses the existing assembler/oracle to rebuild commitments.
Exclusion does not evict mempool entries or refill template capacity.

Shared construction adds coinbase leaves before surviving transaction outputs,
retains the daemon-predicted DNRS, and rebuilds witness/filter commitments.
Nonempty JD remains gated by the existing protocol limitation. No accounting,
PPLNS journal schema, consensus rule, or database schema change was introduced
by the paired recovery patch.

### Issue found and fixed during this review

The shared DNRF builder omitted spent-input scripts. The daemon's full filter
includes them, but ConnectTip currently logs a hash mismatch without rejecting
the accepted block. Previous acceptance-only tests therefore did not prove full
filter correctness; repeated input/output scripts can also mask the omission.

A new regression gave the spent input a script absent from every output. It
failed before the correction and passed afterward. The pool now retains scripts
from successful selected proofs, includes them in DNRF, and treats absent or
malformed script metadata as a per-transaction proof failure. Exclusion rebuilds
discard scripts from the old selection. The real mixed shield/unshield test now
checks the stored coinbase DNRF against the full filter independently of block
acceptance.

Local validation after this correction: **440 workspace tests passed, 9 ignored**;
the strengthened real mixed mining test passed. The pool
[candidate workflow](https://github.com/DineroLabs/dinero-sv2/actions/runs/34972603954)
also passed all **440 native Linux workspace tests** and built both x86_64 and
aarch64 artifacts; its release-publication job was skipped. Pool-scoped
all-targets Clippy passed with only the existing `ops.rs` test-module ordering
lint allowed. A broader workspace Clippy invocation reports pre-existing
`dinero-miner-ux` lints, outside this change.

The daemon
[candidate workflow](https://github.com/DineroLabs/dinero-v8/actions/runs/34971187258)
passed its tarball, Debian package, OpenSSL/dependency and debug-symbol gates.
Its release-attachment step was skipped.

All four process tests passed against the exact packaged x86_64 pair: mixed
shield/unshield inclusion with independent full-filter validation, injected
proof-failure recovery, shared payouts and CPU solo DNRS compatibility. This
artifact test ran in isolated Ubuntu 24.04 under emulation on macOS arm64, with
networking disabled and executable mounts read-only. Source/version identity and
checksums were checked before and after; a negative control rejected the old
daemon even when its file checksums matched its supplied checksum file.

Candidate SHA-256 checksums:

```text
c5e2e6fa726de7bbf9cf6b5288e3fd78f48f1fde4b3db7d7612b6c7600a4cbef  dinerod
afa19dad27bdea236be1ec9560a0f0609b16abdebf7ecf86c25e0b1c7829bc33  dinero-cli
3d9a17f8c202517d848cb431b7a4bff5ef11897bdd200f413192f03d1dcb2054  dinero-sv2-pool
```

Full packages, matching debug symbols, the immutable candidate manifest,
qualification receipt and operator-specific `SJ-ROLLOUT.md` remain locally in
`target/release-candidates-20260915/`. This preparation did not exercise production
chainstate or miner reconnections on SJ, and it is not a native performance
benchmark. Deployment remains owner-gated.

## Operational boundaries

A retained unshield can become invalid under existing anchor rules. This change
does not guarantee eventual confirmation of every excluded transaction.

All configured pool backends need the companion daemon for exclusion recovery.
An old backend can still serve ordinary templates; recovery on it fails closed.
SJ was observed using one RPC URL, `http://127.0.0.1:20998`.

The ops alerting handoff remains separate. A fresh producer heartbeat or generic
template timestamp does not prove that shared miners received usable work.
