# Transaction-bearing solo / job-declaration work

Status: protocol design for review. This document does not enable nonempty
JD jobs. Shared mining already uses complete pool-owned templates.

## Why the current message is insufficient

`CoinbaseContext` supplies coinbase fragments, transaction merkle siblings,
height, total coinbase value and optional DNRS. `UtreexoStateAnnouncement`
supplies the accumulator stump after selected transaction inputs are deleted.
Changing the coinbase changes its txid and leaf hashes. A nonempty block also
needs the remaining transaction output leaves, complete filter inputs and the
witness commitment. None of those can be recovered from a merkle path.

The current pool therefore withholds JD context for nonempty templates and
rejects their extended shares. Keep both guards until the extension below is
implemented and qualified. An empty mempool is not an activation prerequisite
for shared mining or daemon-owned `mining.getjob` work.

## Proposed negotiated context

Use a new capability and new message, with assigned identifiers agreed during
review. Do not append bytes to the existing strictly decoded CoinbaseContext.
Old clients retain coinbase-only JD support. Advertising a capability must mean
that both context parsing and block reconstruction are implemented.

Bind every context to the negotiated channel, current job ID, previous block
hash and candidate height. The context contains:

| Data | Purpose |
| --- | --- |
| Coinbase prefix/suffix and transaction merkle siblings | Recompute txid merkle root after miner payout customization |
| Total coinbase value in una | Exact subsidy plus fees from the selected transaction set |
| Utreexo leaf-version/maturity activation height for this network | Remove the workers' current hardcoded mainnet-height assumption |
| Post-deletion accumulator stump | Starting state before adding coinbase and surviving outputs |
| Surviving non-coinbase output leaf hashes, in block order | Append **after all coinbase leaves**; omit outputs spent inside the same block |
| Required DNRS root and witness merkle root | Construct exact canonical zero-valued commitments; coinbase witness leaf is zero |
| Non-coinbase filter scripts | All eligible transaction output scripts plus chain-backed spent-input scripts; add customized coinbase payout scripts locally |

Preserve exact compact transaction bytes when deriving txids, witness hashes,
merkle paths and output identities. Expanding a compact proof for verification
must never replace the serialization used for these identities.

## Framing and resource bounds

Noise payloads are limited to `65535 - 16 - 6 = 65513` bytes. A large block's
leaf/script list cannot be assumed to fit one frame. Use a begin record followed
by bounded indexed chunks and a final digest, with a reviewed canonical binary
encoding. The begin record declares total bytes, record counts and chunk count.

Before allocating, enforce an absolute context-size cap derived from the
daemon's maximum block/transaction/output/script limits. Review must specify
these numeric limits; do not choose a permissive arbitrary cap. Reject count
overflow, impossible stump shape, oversized scripts, trailing bytes, duplicate,
missing, reordered or cross-job chunks and digest mismatch. Keep at most one
incomplete context per channel with a deadline and aggregate connection budget.

Publish no hashing work until the complete context and matching job have been
validated. A new prev-hash, replacement job, reconnect or channel close discards
the incomplete context. Target-only updates may restart a complete current job;
they must never revive invalidated context. No implicit fallback that drops
transactions, commitments or validation is allowed.

## Independent pool reconstruction

The pool reconstructs submitted coinbase outputs from its own retained template,
not miner-supplied leaves, roots or fees. Check value addition for overflow,
require the exact subsidy-plus-fee sum, and enforce commitment uniqueness,
zero values and exact DNRS/DNRW/DNRF bytes before crediting a share.

Starting from the same post-deletion stump, append the miner's coinbase leaves,
then the surviving transaction leaves. Recompute txid merkle root, Utreexo root
and complete header; enforce current job, timestamp and target. Final block
submission remains subject to the daemon's ordinary consensus validation.

## Required qualification before enabling nonempty JD

1. Codec tests: canonical round trips plus every truncation, trailing bytes,
   oversize lengths, integer overflow, out-of-order/duplicate chunks, wrong
   channel/job/parent and new-tip interruptions.
2. Independent roots: compare pool and miner results against daemon templates
   for transparent spends, shield, transfer, unshield, mixed transactions and
   parent/child packages. Include in-block-spent outputs and reused scripts.
3. Actual CPU and GPU extended shares: mine nonempty blocks above mandatory
   DNRS/DNRW and Utreexo leaf-version heights. Verify payout sum, fees, exact
   witness/filter commitments, output proofs and a spend of an unshield output.
4. Mutations: wrong leaf order, omitted/extra output, wrong network leaf version,
   altered compact byte, missing/duplicate/nonzero commitment, changed fee sum,
   stale context and a target update between chunks must fail before credit.
5. Combined activation, disconnect/reconnect, restart and reorg; compare full
   and stateless nodes. Exercise 30/60/120-second job intervals, bursts and
   delayed jobs under load. Legacy-client behavior must remain explicit.
6. Linux CPU plus physical Metal, CUDA and OpenCL evidence. A compile-only GPU
   result or a Metal pass does not qualify another GPU backend.

This protocol work is separate from changing the PoW algorithm. CPU/GPU hashing
continues to use the existing 128-byte header and SHA-256d.
