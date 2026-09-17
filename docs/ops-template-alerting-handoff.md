# Template availability and mempool alerting handoff

This is an ops monitor scope proposal. No monitor or notification route is enabled
by this document. Seed RPCs and pool ops endpoints should be read with existing
local credentials; production changes and notification routing remain owner-gated.

## Page on unavailable mining work

The pool's process, listening port, and producer heartbeat can all remain healthy
while every candidate is refused. Monitor usable work separately:

- Poll the authenticated pool status endpoint. Track `last_template_at_unix`,
  `template_height`, `template_prev_hash`, and the producer heartbeat/phase.
- Compare template parent/height with a fresh RPC observation from the selected
  daemon. The daemon height cached in pool telemetry is recorded when templates
  succeed; it can remain stale throughout a refusal incident.
- With same-tip refresh enabled (default 15 seconds), use a configurable
  120-second no-success threshold. Handle startup (`last_template_at_unix == 0`)
  with a startup grace interval, and detect daemon/RPC unreachability separately.
- Shared-template construction can fail after `record_template` updates the
  status timestamp. To cover shared miners, also track the explicit
  `build_shared_template failed` / `extract_fee_script failed` log events, or use
  a shared-mode SV2 probe that confirms receipt of valid fresh jobs. The status
  timestamp alone cannot prove that shared clients received usable work.
- Count refusal events over the same window for diagnosis. Current code emits
  `could not build a consistent pool template`, invalid/missing DNRS refusals,
  mapping errors and backend-selection failures. Match the deployed version's
  actual messages; a lone historical log match is not a current outage signal.

Do not make the primary alert depend on connected miners, shares or a nonempty
mempool. An empty-pool service outage should still be detected, and missing shares
may simply reflect idle miners. Treat maintenance and intentional mining-safety
pauses explicitly; report the reason without repeatedly paging for an acknowledged
pause. Track freshness separately for each enabled mining mode.

Persist incident state across monitor restarts. Deduplicate by host, pool and
condition; send one opening notification, controlled reminders/escalation, and one
recovery notification after consecutive healthy observations. Include deployment
version, work age, tip mismatch, refusal count, latest reason and affected mode.
Use rate-limited notifications, not automatic daemon/pool restarts or mempool flushes.

## Warn on aging mempool transactions

`getmempoolinfo` exposes `size` and, for a nonempty mempool,
`oldest_tx_age_seconds`. `getrawmempool [true]` supplies per-transaction `time`,
`height` and `depends`. An empty mempool omits the oldest-age field; a missing
field in an error response must not be treated as an empty mempool.

Record membership and observed tip progression per seed. Warn on a configurable
age threshold, using blocks advanced while the same transaction remained present
as supporting evidence. Escalate prolonged cases separately from the two-minute
work-availability page. Low-fee transactions can age normally, and age alone does
not prove invalidity or that a transaction can never confirm. Shared txids across
seeds should be grouped to avoid one notification per seed for one incident.

## Acceptance tests for the ops implementation

1. Keep process/port/heartbeat healthy while refusing every template: page once
   within the configured two-minute target, allowing for the polling interval.
2. Fail shared construction while generic template timestamps advance: detect
   the shared-mode outage.
3. Restart the monitor during the incident: do not duplicate the opening page.
4. Recover fresh usable jobs: send exactly one recovery notification.
5. Exercise empty mempool, absent fields, RPC timeout, startup grace, clock skew,
   acknowledged maintenance and notification-delivery failure.
6. Keep one tx across advancing blocks and seed observations: produce one grouped
   aging warning; resolve on disappearance with the observed reason, if known.

Code references: pool `src/main.rs` template producer and `src/ops.rs` telemetry;
daemon `src/rpc/methods_mempool_context.cpp`. Recheck capabilities against each
host's deployed binary before enabling the monitor.
