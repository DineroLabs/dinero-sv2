# Run a Dinero pool

A Dinero pool pays every contributing miner **directly in the block's
coinbase**, split by share weight, at the moment the block is found.

That one design choice removes most of what makes running a pool
daunting:

- **You never hold your miners' coins.** There are no balances on your
  books, no payout run, no withdrawal queue. You cannot lose their money
  and you cannot be accused of stealing it.
- **Your fee is verifiable.** It is an output in the block. Any miner can
  check what you took, from the chain, without trusting you.
- **A crash costs nothing but uptime.** There is no ledger to reconcile.

The trade-off: every paid miner needs an output in every block, so this
suits pools of tens of miners, not thousands.

## What you need

- A Linux server (x86_64 or aarch64) with a public IP
- **A `dinerod` node on the same host.** This is not optional — the pool
  gets block templates from it and submits found blocks through it. A
  pool operator is necessarily a node operator. **Budget real time for
  the first sync** — see below.
- A `din1p…` Taproot address for your fee
- One open inbound port (4444 by default)

## 1. Install and sync a node

Get `dinerod` from <https://dinerolabs.org> and start it with RPC
enabled.

A fresh node bootstraps from the published AssumeUTXO snapshot rather
than replaying the whole chain from genesis.

**Be realistic about how long the rest takes.** The snapshot shipped with
the release is not the newest one the fleet publishes — the release
carries an anchor at height 84131, while the automated publisher is
around 99677 — so a fresh node still replays the ~15,000 blocks between
them. Measured on a 2-core VPS: **about 8–9 blocks a minute, roughly 30
hours to reach the tip.** It is unattended, but it is not a coffee break.

(This gap is a packaging problem, not a property of the chain. Once a
release ships an anchor near the publisher's current height, the same
install lands minutes from the tip.)

Confirm you are near the tip before continuing:

```sh
dinero-cli getblockcount
```

Compare against any public node; if you are within a few hundred blocks,
you are ready.

## 2. Install the pool

```sh
curl -fsSL https://raw.githubusercontent.com/DineroLabs/dinero-sv2/main/scripts/install-pool.sh \
  | sudo sh -s -- --payout-address din1pYOURADDRESS
```

This downloads the release binary, verifies its SHA-256, generates your
pool's Noise static key and an ops token, installs a systemd unit, and
starts the service.

Defaults worth knowing:

| Flag | Default | Notes |
|---|---|---|
| `--fee-bps` | `1000` (10%) | Your cut of each block your pool finds. Any value 0–10000 (0–100%) is accepted; 10% is the installer's default, not a cap. Operators compete on this, and miners verify it from the block's coinbase rather than trusting you. |
| `--bind` | `0.0.0.0:4444` | Miner-facing port |
| `--cookie` | `/var/lib/dinero/.cookie` | Node auth — no password in the unit |

## 3. Give miners your public key

**This is the step people miss.** A miner pointed at your pool without
your key still connects — but *unpinned*, on trust-on-first-use, and it
prints a warning. Pinning is what stops someone impersonating your pool.

(The miner ships the reference pool's key as a default, but deliberately
does **not** apply it to a custom pool — a key issued for one pool is
meaningless for another, so it declines to pretend otherwise.)

The installer prints your pubkey. To see it again:

```sh
dinero-sv2-pool --print-pubkey --tp-key /etc/dinero-sv2/pool-static.key
```

Miners then connect with:

```sh
dinero-miner --address <their din1p...> --reward-mode shared \
  --pool your.host:4444 --server-pubkey <your pubkey>
```

Publish the key somewhere miners can check it against. Without
`--server-pubkey` they will see:

```
warning: no server pubkey configured for pool your.host:4444 — connection is unpinned (trust-on-first-use)
```

## 4. Watch it

```sh
journalctl -fu dinero-sv2-pool
```

You want to see `new template`, `accepted shared share`, and eventually
`★ SHARED block ACCEPTED — split across contributors`.

### Operator status endpoint

```sh
curl -H "Authorization: Bearer $(cat /etc/dinero-sv2/ops-token)" \
  http://127.0.0.1:4445/status
```

The response is a versioned contract. `schema_version: 2` reports connected
Stratum sessions separately from PPLNS contributors, daemon block/header
height, template identity and freshness, accepted/rejected share counters,
the last accepted share, the last block-submission result, and rejection
counts grouped by reason. Each contributor's next-block share remains in
basis points. Fields are additive; v1 cockpit clients can continue reading
their original fields, while v2 clients must reject missing or ill-typed v2
health fields instead of displaying false zeroes.

It is **loopback-only and plain HTTP by design** — a pool binary that
handles money should not also carry a TLS stack. For remote access, use
an SSH tunnel:

```sh
ssh -N -L 4445:127.0.0.1:4445 you@your.host
```

or put nginx/caddy in front of it. If you bind it off-loopback, the pool
logs a warning; that is not a substitute for a proxy.

Note the endpoint reports **operations**, not earnings. Your fee is
on-chain — read it from the coinbase of blocks your pool found, which is
the only source that cannot be wrong.

## If it stops

The pool exits non-zero (so systemd restarts it) when its template
producer either **dies** or **wedges** — the second case being a producer
that is still alive but stuck, which would otherwise leave the pool
accepting miners while serving stale jobs forever.

The wedge threshold is `--template-stall-secs`, default 600. It is
deliberately far above worst-case iteration time; the node RPC times out
at 15s per request, so even several failing calls in a row stay well
under it.

If you see repeated restarts, the node is usually the cause — check that
`dinerod` is running, synced, and answering RPC.

## Changing your fee address

The address you passed to the installer receives your operator fee. You can
change it two ways.

**Edit the unit** (always available, needs a restart):

```sh
sudoedit /etc/systemd/system/dinero-sv2-pool.service   # change --payout-address
sudo systemctl daemon-reload && sudo systemctl restart dinero-sv2-pool
```

**From a client such as dinero-qt** (opt-in). This is off unless you asked
for it at install time:

```sh
curl -fsSL .../install-pool.sh | sudo sh -s -- \
  --payout-address din1p... --allow-payout-change
```

Understand what you are turning on. Your ops token normally only *reads*
status. With this enabled, anyone holding it can retarget your fee output —
so treat the token like a key, not a password, and keep the endpoint on
loopback or behind an SSH tunnel. It cannot touch your **miners'** payouts:
contributor outputs come from the PPLNS window, which no ops route reaches.

Two safeguards are built in:

* A candidate address is proven against a real `getblocktemplate` before it
  is adopted. A typo is rejected and your old address keeps mining — without
  this a bad address would fail every template call and your pool would
  quietly serve nothing.
* The new address is written to `/etc/dinero-sv2/payout-address`, which wins
  over the unit file on the next start. Otherwise a restart would silently
  revert your fee to the installer's address.

By hand, the same thing:

```sh
curl -X POST -H "Authorization: Bearer $(cat /etc/dinero-sv2/ops-token)" \
  -H 'Content-Type: application/json' \
  -d '{"address":"din1pYOURNEWADDRESS"}' \
  http://127.0.0.1:4445/payout-address
```

`GET /status` reports `payout_address`, so you can confirm what is actually
live rather than trusting the unit file.

## What you actually earn

Your fee is a percentage of blocks **your pool** finds. With no miners,
that is nothing. At this network's size, running a pool is not a
business — the reasons to do it are that you stop depending on someone
else's server, you choose your own transaction selection, and you can
pool with people who never have to trust you.
