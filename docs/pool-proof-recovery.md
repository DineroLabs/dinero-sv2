# Pool proof and shielded transaction recovery

## Findings

The pool's existing empty-input guard already skips proof RPCs for transactions
with zero transparent inputs. The September incident exposed additional gaps:

- The pool sent `[{"outpoints":[...]}]` to `getutxoproofs_batch`, whose contract
  is `[[{"txid":...,"vout":...}]]`. That endpoint also returns nested proofs
  without the leaf hash the pool expected.
- `getproofupdates` accepts the object request and returns complete flat proofs.
  Its limit is 100 outpoints, so requests are chunked and each proof is checked
  against the captured forest root, leaf count, and outpoint.
- Shared templates rejected every nonempty transaction set and submitted an
  empty transaction list. They now retain the exact job's transaction bytes,
  compute DNRW/DNRF for that set, and append accumulator outputs in consensus
  order: coinbase first, then transactions. Outputs spent inside the block are
  omitted from the forest and require no chain-tip proof.

## Recovery contract and deployment dependency

On a per-transaction proof failure, the pool excludes that transaction and its
transparent descendants. It requests `getblocktemplate` with the original
payout `address` and an `exclude_txids` array. The daemon must apply exclusions
before assembling the coinbase and commitments. The pool verifies that none
of those IDs survived, the parent and height stayed fixed, and DNRS remains
present when required. Four attempts and a 45-second overall timeout bound
recovery. It never edits a daemon coinbase to pretend a transaction was removed.

The companion daemon extension is implemented in dinero-v8 commit
`b53bc958b3360ca9b8f52aa0488714fec51e68f0` on
`codex/pool-template-exclusions`. Its assembler removes requested transactions
and transparent descendants before commitment generation, recalculates fees,
and leaves the mempool intact. IDs must be 64 hexadecimal characters; the
request limit is 10,000 entries. Exclusion does not refill template capacity.

The DNRS CI workflow pins that daemon commit and runs the daemon regressions
and optimized pool recovery tests. Both development branches were published in
daemon-first order so the pinned checkout resolves. Neither component has been
deployed. Existing backends ignore the extra request field; the pool detects
that and refuses recovery. Ordinary mixed shield/unshield inclusion works with
the existing v8.1.12 daemon.

A parent shielded tree root is not a replacement DNRS. DNRS covers the full
shielded state, including nullifiers and height-dependent anchor history.
Even an empty block advances that history. `getsynchealth`'s persisted tree
marker and `daemon.shieldedroot`'s current full digest are not the next-block
prediction oracle. The daemon must compute the post-block commitment.

The current JD wire format lacks transaction output leaves and filter scripts.
For nonempty templates the pool serves shared and standard daemon-owned work,
but withholds JD context so reference solo/JD clients cannot construct invalid
headers. Coinbase-only JD work remains supported. Extending JD for nonempty
blocks requires a separate protocol/client change.

## Validation

Tests were added before the fixes. The old implementation failed the proof
RPC contract, shared nonempty template, and proof failure recovery regressions.
The zero-transparent-input test passed before changes, correcting the diagnosis.

The regression suite covers no-input unshields, mixed inputs, intra-block spends,
proof identity/root/shape errors, the 100-outpoint batch limit, dependency
exclusions, an old backend ignoring exclusions, and shared commitments/output
order/retained transaction bytes.

Real process test (isolated regtest, v8.1.12 daemon):

```sh
DINEROD_BIN=/absolute/path/to/dinerod cargo test -p dinero-sv2-pool \
  --test shared_split_e2e \
  shared_pool_confirms_unshield_with_transparent_inputs_in_same_template \
  -- --ignored --nocapture
```

It mines 101 setup blocks, shields a note, confirms it, then puts an unshield
and a new shield in the same template. A real Noise/SV2 shared miner connects
to the pool process and finds block 103. The daemon accepts both transactions;
its resulting full shielded state digest matches the original template's DNRS.
No production node, wallet, pool service, or mempool was changed.

Recorded validation on this branch:

- `cargo test --locked --workspace`: 438 passed, 9 opt-in tests ignored.
- Explicit real-process tests passed: mixed unshield/shield inclusion (height
  103), shared contributor payouts (height 26), CPU solo DNRS (height 2).
- Strict Clippy encounters an existing `items_after_test_module` warning in
  unchanged `src/ops.rs`. Pool all-targets Clippy passes with only that lint
  allowed and all other warnings denied.
- Workspace formatting has pre-existing differences in unrelated files;
  changed Rust files pass `rustfmt --check` with child-module traversal disabled.
- Linux qualification is recorded in
  [the qualification report](pool-proof-linux-qualification.md).
  Production rollout has not been performed.

### Companion daemon recovery validation

```sh
DINEROD_BIN=/absolute/path/to/patched/dinerod cargo test --locked \
  -p dinero-sv2-pool --test shared_split_e2e \
  shared_pool_recovers_from_bad_proof_with_daemon_rebuilt_dnrs \
  -- --ignored --nocapture
```

This regression fails against the old daemon at the exclusion-support check.
With the companion patch, a local proxy forwards real RPCs and deliberately
fails an otherwise valid input proof for a shield transaction. The actual pool
requests that transaction's exclusion and mines the remaining unshield in block
103. The daemon accepts the block with a rebuilt DNRS; the failed shield remains
in the mempool. Both this recovery test and normal mixed inclusion pass against
the patched daemon.

Daemon tests separately validate malformed parameters, duplicate/uppercase and
unknown IDs, descendant closure, fee subtraction, unchanged ordinary selection,
and externally accepted filtered and all-excluded blocks. A retained shield is
then confirmed through an ordinary template. Existing daemon GBT/coordinator
mining, DNRS persistence and dormant controls also pass.

Mempool retention does not guarantee future transaction validity: tests observed
that an unshield's old anchor can become invalid after another shielded tree
update. Recovery preserves the existing validation rules and does not repair or
rebroadcast such transactions.
