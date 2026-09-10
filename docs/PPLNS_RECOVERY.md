# PPLNS recovery, pool 0.1.6

Recovery loads a version-1 exact-window checkpoint without eviction, then
applies each appended share through the same record/eviction path as live work.
One journal lock orders persistence, live credits and compaction. The window
lock is released before disk I/O. Shares are acknowledged only after a flushed,
synced append; a failed writer refuses further credits until restart/recovery.
Compaction syncs a replacement before rename and syncs the directory on Unix.
A torn final append is discarded and the recovered prefix is checkpointed on
startup. Complete corrupt records fail startup rather than erase credit.

## Upgrade and rollback

Legacy JSONL has no checkpoint boundary. It can contain a compacted suffix plus
later shares, so replay alone cannot promise exact recovery. Nonempty legacy
files are refused. Do not delete them to get the service started.

Capture the running pool's window entry count and every contributor weight from
the authenticated ops endpoint. While the old process is quiescent, identify
its journal suffix and verify count and all weights against that snapshot. Stop
cleanly, verify no append occurred, back up the full journal and binary, and
atomically write one newline-terminated JSON object:

    {"checkpoint_version":1,"entries":[...exact verified live entries...]}

Preserve journal ownership/permissions. Start 0.1.6 and verify checkpoint entries
and weights before allowing new credits to obscure the comparison. Keep the
old full journal for evidence; never overwrite subsequent credits with it.

Old binaries cannot interpret the new checkpoint. A rollback must first recover
the CURRENT checkpoint plus appends and export the CURRENT live window in legacy
format, checking old restore semantics preserve that window. If that cannot be
proven, keep the pool stopped rather than dropping credits or restoring an old
backup. The old binary-only rollback procedure is no longer suitable.

Regression evidence covers rate changes, seven contributors, variable weights,
repeated recovery/compaction, concurrent credits, torn writes, corrupt records,
failed writers and interrupted compaction. Full-history example: live 2,414
entries versus 782 under the old one-pass restore. Reward rules are unchanged.
