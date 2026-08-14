#!/bin/sh
# Install the Dinero miner: detects OS/arch, downloads the latest miner-v*
# release asset, verifies SHA-256, installs as ~/.local/bin/dinero-miner.
# DINERO_MINER_VERSION=miner-vX.Y.Z overrides "latest".
set -eu
REPO="DineroLabs/dinero-sv2"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  T="aarch64-apple-darwin" ;;
  Linux-x86_64)  T="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64) T="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)"; exit 1 ;;
esac
TAG="${DINERO_MINER_VERSION:-$(curl -fsSL https://api.github.com/repos/$REPO/releases | \
  grep -o '"tag_name": *"miner-v[^"]*"' | head -1 | cut -d'"' -f4)}"
[ -n "$TAG" ] || { echo "no miner release found"; exit 1; }
BASE="https://github.com/$REPO/releases/download/$TAG"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/miner" "$BASE/dinero-sv2-miner-$T"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"
( cd "$TMP" && grep "dinero-sv2-miner-$T\$" SHA256SUMS | sed "s|dinero-sv2-miner-$T|miner|" | \
  { shasum -a 256 -c - 2>/dev/null || sha256sum -c -; } ) || { echo "checksum mismatch"; exit 1; }
mkdir -p "$HOME/.local/bin"
install -m 755 "$TMP/miner" "$HOME/.local/bin/dinero-miner"
echo "installed: $HOME/.local/bin/dinero-miner ($TAG)"
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo 'add to PATH: export PATH="$HOME/.local/bin:$PATH"' ;; esac
echo "run: dinero-miner"
