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

# --- argument parsing, exercised by actually RUNNING the script ------------
# `sh -n` cannot catch a variable that is referenced but never assigned, and
# under `set -u` that is a hard runtime failure deep in the install path. So
# run the real script: with valid args it must get PAST parsing and stop at
# the root check (exit 1), not at "unknown option" (exit 2).
run_args() { sh "$SCRIPT" "$@" 2>&1; }

out=$(run_args --allow-payout-change --payout-address din1pgood || true)
case "$out" in
  *"run as root"*) echo "ok   - --allow-payout-change is a recognised option" ;;
  *"unknown option"*) echo "FAIL - --allow-payout-change not parsed: $out"; fails=$((fails+1)) ;;
  *) echo "FAIL - unexpected: $out"; fails=$((fails+1)) ;;
esac

out=$(run_args --allow-fee-change --payout-address din1pgood || true)
case "$out" in
  *"run as root"*) echo "ok   - --allow-fee-change is a recognised option" ;;
  *"unknown option"*) echo "FAIL - --allow-fee-change not parsed: $out"; fails=$((fails+1)) ;;
  *) echo "FAIL - unexpected: $out"; fails=$((fails+1)) ;;
esac

out=$(run_args --help || true)
case "$out" in
  *--allow-payout-change*--allow-fee-change*) echo "ok   - --help documents both mutation flags" ;;
  *) echo "FAIL - --help omits the new flag"; fails=$((fails+1)) ;;
esac

# Every variable the script expands must have an UNCONDITIONAL top-level
# assignment before first use. A `case` arm like `--flag) VAR=yes` does not
# count: without the flag the variable is unset, and `set -u` then kills the
# install midway, after it has already written files. Anchoring at column 0
# is what distinguishes the two.
for v in ALLOW_PAYOUT_CHANGE ALLOW_FEE_CHANGE ALLOW_FLAG ALLOW_FEE_FLAG PAYOUT FEE_BPS BIND RPC_URL COOKIE START; do
  first_use=$(grep -nF "\$$v" "$SCRIPT" | head -1 | cut -d: -f1)
  first_set=$(grep -nE "^$v=" "$SCRIPT" | head -1 | cut -d: -f1)
  if [ -z "$first_use" ]; then
    echo "ok   - $v (never expanded)"
  elif [ -n "$first_set" ] && [ "$first_set" -le "$first_use" ]; then
    echo "ok   - $v unconditionally assigned before use"
  else
    echo "FAIL - $v expanded at line ${first_use:-?}, no top-level assignment before it (found: ${first_set:-none})"
    fails=$((fails + 1))
  fi
done

[ "$fails" -eq 0 ] || { echo; echo "$fails test(s) failed"; exit 1; }
echo; echo "all install-pool banner tests passed"
