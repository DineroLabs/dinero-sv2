#!/bin/sh
# Install a Dinero SV2 pool: downloads the latest pool-v* release asset,
# verifies SHA-256, generates the pool's Noise static key and ops token,
# and installs a systemd unit.
#
# Running a pool means running a NODE too — the pool gets block templates
# and submits blocks through `dinerod`'s RPC. This script checks for one
# and refuses rather than installing a half-stack that cannot serve.
#
#   curl -fsSL .../install-pool.sh | sudo sh -s -- --payout-address din1p...
#
# DINERO_POOL_VERSION=pool-vX.Y.Z overrides "latest".
set -eu

REPO="DineroLabs/dinero-sv2"
PAYOUT=""
FEE_BPS="1000"
BIND="0.0.0.0:4444"
RPC_URL="http://127.0.0.1:20998"
COOKIE="/var/lib/dinero/.cookie"
START="yes"
ALLOW_PAYOUT_CHANGE="no"

usage() {
  cat <<USAGE
usage: install-pool.sh --payout-address din1p... [options]

  --payout-address ADDR   where your operator fee is paid (required)
  --fee-bps N             operator fee in basis points (default 1000 = 10%)
  --bind HOST:PORT        miner-facing listen address (default 0.0.0.0:4444)
  --rpc-url URL           dinerod RPC (default http://127.0.0.1:20998)
  --cookie PATH           dinerod auth cookie (default /var/lib/dinero/.cookie)
  --no-start              install but do not start the service
  --allow-payout-change   let the ops endpoint change your fee address at
                          runtime (e.g. from dinero-qt). OFF by default:
                          enabling it means your ops token can redirect
                          YOUR fee output. Miners' payouts are unaffected.
USAGE
}

# The pool's Noise static key is created on first touch; --print-pubkey both
# creates and reads it. Operators must publish this so miners can pin it.
derive_pubkey() { # <binary> <key-path> <payout-address>
  # --payout-address is a REQUIRED arg on the binary, so clap rejects a bare
  # --print-pubkey with exit 2 and an empty stdout. Pass it even though
  # printing a key ignores it, or the operator never sees their pubkey.
  "$1" --print-pubkey --tp-key "$2" --payout-address "$3" 2>/dev/null | tr -d '\r\n '
}

# Host string printed in the miner command line.
pool_host() {
  # `hostname -f` EXITS 0 with a bare label (e.g. "DineroTX") when the box has
  # no domain, so the `||` fallback never fires and miners are handed a name
  # they cannot resolve. Require a dot before trusting it.
  _h=$(hostname -f 2>/dev/null || true)
  case "$_h" in
    *.*) echo "$_h" ;;
    *)   echo "YOUR_HOST" ;;
  esac
}

# Sourced by tests to reach the functions above without running the install.
[ "${INSTALL_POOL_LIB_ONLY:-}" = "1" ] && return 0 2>/dev/null || :

while [ $# -gt 0 ]; do
  case "$1" in
    --payout-address) PAYOUT="${2:?}"; shift 2 ;;
    --fee-bps)        FEE_BPS="${2:?}"; shift 2 ;;
    --bind)           BIND="${2:?}"; shift 2 ;;
    --rpc-url)        RPC_URL="${2:?}"; shift 2 ;;
    --cookie)         COOKIE="${2:?}"; shift 2 ;;
    --no-start)       START="no"; shift ;;
    --allow-payout-change) ALLOW_PAYOUT_CHANGE="yes"; shift ;;
    -h|--help)        usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

[ "$(id -u)" = "0" ] || { echo "run as root (writes /etc, /var/lib, systemd)" >&2; exit 1; }
[ -n "$PAYOUT" ] || { echo "--payout-address is required" >&2; usage; exit 2; }
case "$PAYOUT" in din1p*) ;; *) echo "payout address must be a din1p... Taproot address" >&2; exit 2 ;; esac
command -v systemctl >/dev/null 2>&1 || { echo "systemd required" >&2; exit 1; }

# --- the node prerequisite -------------------------------------------------
# A pool with no node has nothing to hand miners. Fail loudly here rather
# than installing something that starts, looks fine, and serves nobody.
if ! command -v dinerod >/dev/null 2>&1 && [ ! -x /usr/bin/dinerod ]; then
  cat >&2 <<'NONODE'
error: no `dinerod` found.

A pool cannot run without a node — it needs one for block templates and
to submit the blocks it finds. Install and sync a node first:

    https://dinerolabs.org   (install script + latest release)

A fresh node bootstraps from the published AssumeUTXO snapshot, so this
is minutes, not hours. Re-run this installer once `dinerod` is on PATH.
NONODE
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  T="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64) T="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m) (pool ships Linux-only)" >&2; exit 1 ;;
esac

TAG="${DINERO_POOL_VERSION:-$(curl -fsSL https://api.github.com/repos/$REPO/releases | \
  grep -o '"tag_name": *"pool-v[^"]*"' | head -1 | cut -d'"' -f4)}"
[ -n "$TAG" ] || { echo "no pool release found" >&2; exit 1; }

BASE="https://github.com/$REPO/releases/download/$TAG"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
echo "downloading $TAG ($T)..."
curl -fsSL -o "$TMP/pool" "$BASE/dinero-sv2-pool-$T"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"
( cd "$TMP" && grep "dinero-sv2-pool-$T\$" SHA256SUMS | sed "s|dinero-sv2-pool-$T|pool|" | \
  { sha256sum -c - 2>/dev/null || shasum -a 256 -c -; } ) >/dev/null \
  || { echo "checksum mismatch — refusing to install" >&2; exit 1; }

install -m 755 "$TMP/pool" /usr/local/bin/dinero-sv2-pool
mkdir -p /etc/dinero-sv2 /var/lib/dinero-sv2
chmod 700 /etc/dinero-sv2

# --- ops token -------------------------------------------------------------
# Kept in a file, not the unit, so it never appears in `ps` or `systemctl cat`.
if [ ! -s /etc/dinero-sv2/ops-token ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > /etc/dinero-sv2/ops-token
  chmod 600 /etc/dinero-sv2/ops-token
  echo "generated ops token: /etc/dinero-sv2/ops-token"
fi

# --- Noise static key ------------------------------------------------------
# The pool generates this itself on first touch; --print-pubkey is how we
# both create and read it. Miners must pin the pubkey.
PUBKEY=$(derive_pubkey /usr/local/bin/dinero-sv2-pool /etc/dinero-sv2/pool-static.key "$PAYOUT" || true)
chmod 600 /etc/dinero-sv2/pool-static.key 2>/dev/null || true

# --- unit ------------------------------------------------------------------
curl -fsSL -o "$TMP/unit" "$BASE/dinero-sv2-pool.service" 2>/dev/null || true
if [ ! -s "$TMP/unit" ]; then
  echo "warning: unit not in release assets; writing a built-in copy" >&2
  cat > "$TMP/unit" <<'FALLBACK'
[Unit]
Description=Dinero SV2 Pool Server
After=network-online.target dinero.service
Wants=network-online.target
[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/dinero-sv2-pool --bind __BIND__ --rpc-url __RPC_URL__ --cookie __COOKIE__ --payout-address __PAYOUT_ADDRESS__ --tp-key /etc/dinero-sv2/pool-static.key --pplns-journal /var/lib/dinero-sv2/pplns-journal.jsonl --ops-bind 127.0.0.1:4445 --ops-token-file /etc/dinero-sv2/ops-token --payout-address-file /etc/dinero-sv2/payout-address --shared-fee-bps __FEE_BPS____OPS_ALLOW_PAYOUT_CHANGE__
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
FALLBACK
fi
# Substituted into the unit as a whole flag, or as nothing. Kept to ONE line
# with no continuation: a newline in a sed replacement would break the script,
# and systemd is happy with the flag appended to the last ExecStart line.
ALLOW_FLAG=""
if [ "$ALLOW_PAYOUT_CHANGE" = "yes" ]; then
  ALLOW_FLAG=" --ops-allow-payout-change"
fi

sed -e "s|__PAYOUT_ADDRESS__|$PAYOUT|g" \
    -e "s|__OPS_ALLOW_PAYOUT_CHANGE__|$ALLOW_FLAG|g" \
    -e "s|__FEE_BPS__|$FEE_BPS|g" \
    -e "s|__BIND__|$BIND|g" \
    -e "s|__RPC_URL__|$RPC_URL|g" \
    -e "s|__COOKIE__|$COOKIE|g" \
    -e "s|--bind 0.0.0.0:4444|--bind $BIND|" \
    -e "s|--rpc-url http://127.0.0.1:20998|--rpc-url $RPC_URL|" \
    -e "s|--cookie /var/lib/dinero/.cookie|--cookie $COOKIE|" \
    "$TMP/unit" > /etc/systemd/system/dinero-sv2-pool.service

systemctl daemon-reload
systemctl enable dinero-sv2-pool >/dev/null 2>&1 || true
if [ "$START" = "yes" ]; then
  systemctl restart dinero-sv2-pool
  sleep 3
  systemctl is-active --quiet dinero-sv2-pool \
    && echo "pool is running" \
    || { echo "pool failed to start — journalctl -u dinero-sv2-pool -n 50" >&2; exit 1; }
fi

cat <<DONE

──────────────────────────────────────────────────────────────────────
  Dinero pool installed ($TAG)

  Your pool's public key (miners MUST pin this):

      ${PUBKEY:-<run: dinero-sv2-pool --print-pubkey --tp-key /etc/dinero-sv2/pool-static.key --payout-address $PAYOUT>}

  Miners connect with:

      dinero-miner --address <their din1p...> --reward-mode shared \\
        --pool $(pool_host):${BIND##*:} \\
        --server-pubkey ${PUBKEY:-YOUR_POOL_PUBKEY}

  Without --server-pubkey they still connect, but unpinned
  (trust-on-first-use) and with a warning. Pinning is what stops
  someone impersonating your pool, so publish the key above.

  Your operator fee is ${FEE_BPS} bps ($(( FEE_BPS / 100 ))%), paid to
  $PAYOUT as an output in every block your pool finds. Your miners can
  verify it on-chain — you never hold their coins.

  Fee address changes made at runtime are saved to
  /etc/dinero-sv2/payout-address, which wins over the unit on restart.
$(if [ "$ALLOW_PAYOUT_CHANGE" = "yes" ]; then
printf '%s\n' "" \
  "  NOTE: --allow-payout-change is ON. Anyone holding your ops token can" \
  "  retarget your fee address. Treat that token like a key, not a password."
else
printf '%s\n' "" \
  "  Changing the fee address from a client (dinero-qt) is OFF. Re-run this" \
  "  installer with --allow-payout-change to enable it."
fi)

  Operator status (loopback only, plain HTTP by design):

      curl -H "Authorization: Bearer \$(cat /etc/dinero-sv2/ops-token)" \\
        http://127.0.0.1:4445/status

  Expose it remotely ONLY behind a TLS reverse proxy or an SSH tunnel:

      ssh -N -L 4445:127.0.0.1:4445 root@$(pool_host)

  Open port ${BIND##*:} to miners.   Logs: journalctl -fu dinero-sv2-pool
──────────────────────────────────────────────────────────────────────
DONE
