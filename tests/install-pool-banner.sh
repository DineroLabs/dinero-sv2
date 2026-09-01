#!/bin/sh
# Regression tests for the two install-pool.sh banner facts an operator
# cannot proceed without: their pool's Noise public key, and a host string
# their miners can actually resolve.
#
# Sources install-pool.sh with INSTALL_POOL_LIB_ONLY=1, which returns before
# any download / systemd / filesystem side effect, so this runs anywhere.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SCRIPT="$HERE/../scripts/install-pool.sh"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
fails=0

check() { # check <name> <expected> <actual>
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"
  else
    echo "FAIL - $1"
    echo "         expected: [$2]"
    echo "         actual:   [$3]"
    fails=$((fails + 1))
  fi
}

INSTALL_POOL_LIB_ONLY=1 . "$SCRIPT"

# --- derive_pubkey ---------------------------------------------------------
# The real binary declares --payout-address as REQUIRED, so clap rejects a
# bare `--print-pubkey` with exit 2 and prints nothing on stdout. This stub
# reproduces exactly that contract.
KEYHEX=8d524e35aed3bcab1324bec82e0d3c0f624d37e02b118363ec179b7a6c23ce61
cat > "$TMP/pool-stub" <<STUB
#!/bin/sh
for a in "\$@"; do [ "\$a" = "--payout-address" ] && { echo $KEYHEX; exit 0; }; done
echo "error: the following required arguments were not provided:" >&2
echo "  --payout-address <PAYOUT_ADDRESS>" >&2
exit 2
STUB
chmod +x "$TMP/pool-stub"

check "derive_pubkey satisfies the binary's required --payout-address" \
  "$KEYHEX" "$(derive_pubkey "$TMP/pool-stub" "$TMP/k" din1ptest)"

# --- pool_host -------------------------------------------------------------
# `hostname -f` succeeds on a box with no domain and returns a bare label
# (DineroTX). Emitting that gives miners an unresolvable --pool argument, so
# a name without a dot must degrade to the YOUR_HOST placeholder.
mkdir -p "$TMP/bin"
mk_hostname() { printf '#!/bin/sh\n%s\n' "$1" > "$TMP/bin/hostname"; chmod +x "$TMP/bin/hostname"; }

mk_hostname 'echo DineroTX'
check "pool_host rejects a bare hostname" \
  "YOUR_HOST" "$(PATH=$TMP/bin:$PATH pool_host)"

mk_hostname 'echo pool.dinerolabs.org'
check "pool_host keeps a real FQDN" \
  "pool.dinerolabs.org" "$(PATH=$TMP/bin:$PATH pool_host)"

mk_hostname 'exit 1'
check "pool_host survives hostname failing outright" \
  "YOUR_HOST" "$(PATH=$TMP/bin:$PATH pool_host)"

[ "$fails" -eq 0 ] || { echo; echo "$fails test(s) failed"; exit 1; }
echo; echo "all install-pool banner tests passed"
