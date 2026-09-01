#!/bin/sh
# Install the Dinero GPU miner: detects OS/arch, downloads the latest miner-v*
# GPU release asset, verifies SHA-256, installs as ~/.local/bin/dinero-gpu-miner.
#
# Deliberately a SEPARATE script from install.sh rather than a --gpu flag on it.
# CPU and GPU are two different binaries with different start commands (the GPU
# miner has no --threads), so a miner should know which one they installed.
# The two live side by side and do not overwrite each other.
#
# DINERO_MINER_VERSION=miner-vX.Y.Z overrides "latest".
set -eu
REPO="DineroLabs/dinero-sv2"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  T="aarch64-apple-darwin" ;;
  Darwin-x86_64) T="x86_64-apple-darwin" ;;
  Linux-x86_64)  T="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64)
    # The release publishes CPU for linux-aarch64 but not GPU. Say so plainly
    # rather than 404 on the download.
    echo "no GPU miner is published for Linux aarch64." >&2
    echo "use the CPU miner instead:" >&2
    echo "  curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install.sh | sh" >&2
    exit 1 ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)"; exit 1 ;;
esac
TAG="${DINERO_MINER_VERSION:-$(curl -fsSL https://api.github.com/repos/$REPO/releases | \
  grep -o '"tag_name": *"miner-v[^"]*"' | head -1 | cut -d'"' -f4)}"
[ -n "$TAG" ] || { echo "no miner release found"; exit 1; }
BASE="https://github.com/$REPO/releases/download/$TAG"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/gpuminer" "$BASE/dinero-sv2-gpu-miner-$T"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"
( cd "$TMP" && grep "dinero-sv2-gpu-miner-$T\$" SHA256SUMS | sed "s|dinero-sv2-gpu-miner-$T|gpuminer|" | \
  { shasum -a 256 -c - 2>/dev/null || sha256sum -c -; } ) || { echo "checksum mismatch"; exit 1; }
mkdir -p "$HOME/.local/bin"
install -m 755 "$TMP/gpuminer" "$HOME/.local/bin/dinero-gpu-miner"
echo "installed: $HOME/.local/bin/dinero-gpu-miner ($TAG)"
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo 'add to PATH: export PATH="$HOME/.local/bin:$PATH"' ;; esac
echo "run: dinero-gpu-miner"
