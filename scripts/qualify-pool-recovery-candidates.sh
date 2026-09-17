#!/usr/bin/env bash
# Run only in an isolated Linux environment; these tests create regtest wallets
# and local daemon/pool/miner processes. Inputs are already-built artifacts.
set -euo pipefail
if [ "$#" -ne 3 ]; then
  echo "usage: $0 BUNDLE_DIR TEST_TOOLS_DIR EVIDENCE_DIR" >&2
  exit 2
fi
CANDIDATE_BUNDLE=$(realpath "$1")
CANDIDATE_TOOLS=$(realpath "$2")
mkdir -p "$3"
CANDIDATE_EVIDENCE=$(realpath "$3")
(cd "$CANDIDATE_BUNDLE" && sha256sum -c SHA256SUMS)
(cd "$CANDIDATE_TOOLS" && sha256sum -c SHA256SUMS)

export DINEROD_BIN="$CANDIDATE_BUNDLE/dinerod"
export DINEROPOOL_BIN="$CANDIDATE_BUNDLE/dinero-sv2-pool"
export DINEROMINER_BIN="$CANDIDATE_TOOLS/dinero-sv2-miner"
"$DINEROD_BIN" --version --json > "$CANDIDATE_EVIDENCE/daemon-version.txt" 2>&1
"$DINEROPOOL_BIN" --version > "$CANDIDATE_EVIDENCE/pool-version.txt"
python3 - "$CANDIDATE_BUNDLE" "$CANDIDATE_TOOLS" "$CANDIDATE_EVIDENCE" <<'PY'
import json, pathlib, sys
bundle, tools, evidence = map(pathlib.Path, sys.argv[1:])
manifest = json.loads((bundle / 'candidate-manifest.json').read_text())
test_source = json.loads((tools / 'source.json').read_text())
assert test_source['commit'] == manifest['pool']['commit']
assert test_source['version'] == manifest['pool']['version']
assert test_source['target'] == 'x86_64-unknown-linux-gnu'
daemon_version = (evidence / 'daemon-version.txt').read_text()
assert f"dinerod {manifest['daemon']['version']}\n" in daemon_version
assert f"commit: {manifest['daemon']['commit']}\n" in daemon_version
assert '-dirty' not in daemon_version, daemon_version
assert (evidence / 'pool-version.txt').read_text().strip() == f"dinero-sv2-pool {manifest['pool']['version']}"
for name in ['dinerod', 'dinero-sv2-pool']:
    header = (bundle / name).open('rb').read(20)
    assert header[:6] == b'\x7fELF\x02\x01', name
    assert int.from_bytes(header[18:20], 'little') == 62, name
print('Packaged binary source, version and x86_64 ELF identities verified')
PY

for CANDIDATE_COMPONENT in dinerod dinero-sv2-pool; do
  CANDIDATE_ALLOWED="ld-linux-x86-64.so.2 libc.so.6 libgcc_s.so.1 libm.so.6"
  if [ "$CANDIDATE_COMPONENT" = dinerod ]; then
    CANDIDATE_ALLOWED="$CANDIDATE_ALLOWED libstdc++.so.6 libudev.so.1"
  fi
  objdump -p "$CANDIDATE_BUNDLE/$CANDIDATE_COMPONENT" > "$CANDIDATE_EVIDENCE/$CANDIDATE_COMPONENT-elf.txt"
  for CANDIDATE_DEPENDENCY in $(awk '/NEEDED/{print $2}' "$CANDIDATE_EVIDENCE/$CANDIDATE_COMPONENT-elf.txt"); do
    case " $CANDIDATE_ALLOWED " in
      *" $CANDIDATE_DEPENDENCY "*) ;;
      *) echo "Unexpected dependency: $CANDIDATE_COMPONENT: $CANDIDATE_DEPENDENCY" >&2; exit 1 ;;
    esac
  done
done

for CANDIDATE_TEST in \
  shared_pool_confirms_unshield_with_transparent_inputs_in_same_template \
  shared_pool_recovers_from_bad_proof_with_daemon_rebuilt_dnrs \
  shared_block_coinbase_pays_window_contributors \
  solo_miner_preserves_dnrs_through_pool; do
  timeout 180 "$CANDIDATE_TOOLS/shared_split_e2e" "$CANDIDATE_TEST" \
    --exact --ignored --nocapture --test-threads=1 \
    2>&1 | tee "$CANDIDATE_EVIDENCE/$CANDIDATE_TEST.log"
  grep -F 'test result: ok. 1 passed; 0 failed' "$CANDIDATE_EVIDENCE/$CANDIDATE_TEST.log"
  grep -F "pool process executable: $DINEROPOOL_BIN" "$CANDIDATE_EVIDENCE/$CANDIDATE_TEST.log"
done

(cd "$CANDIDATE_BUNDLE" && sha256sum -c SHA256SUMS)
echo 'PASS: all four tests used the unchanged packaged candidate binaries'
