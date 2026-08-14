# dinero-sv2-miner: idiot-proof interactive CLI + public distribution (design)

Date: 2026-08-14
Status: draft for owner review
Origin: DineroMiner GUI work (DineroLabs/DineroMiner) proved the pipeline —
five real mainnet blocks mined through `dinero-sv2-miner --json` on
2026-08-14. This spec brings the same "paste an address, press Enter" bar to
the bare terminal on any machine.

## Purpose

Anyone with a terminal — macOS, Windows, Linux, or a Chromebook's Crostini
shell — installs with one pasted command and mines to any Dinero address with
zero flags and zero chain state:

```
$ dinero-miner
  paste your Dinero address (din1p…): <paste> ⏎
  ✓ valid — saved for next time
  connected · pool 173.249.200.59:4444 · shared rewards · 7 threads
  ⛏  2.09 MH/s   shares 14 ok / 0 rej   blocks 1   up 3m12s
  ■ block found  #1  14:22:07
    hash   000000574714…7fd3b995
    nonce  0x014b216a
    tries  21,700,970
```

No node in either reward mode: the pool's node serves templates. `shared` =
PPLNS split; `solo` = miner-owned coinbase via the pool (whole reward if your
hash wins). This is the core promise and must appear in the README verbatim.

## Approach: ONE binary, upstream

All UX lands inside `dinero-sv2-miner` (and mirrored in
`dinero-sv2-gpu-miner`) rather than a wrapper: a single static binary is the
most idiot-proof distribution there is, and the GUI (DineroMiner) keeps
consuming the same binary's `--json` mode unchanged.

## Changes to the miners (crates/dinero-sv2-miner, mirrored in the GPU crate)

1. **`--address <din1p…>`** — accepts a bech32m Taproot address and derives
   the 34-byte P2TR script internally (port `payout_script_hex` +
   `validate_address` from DineroMiner `src-tauri/src/address.rs`, including
   its typed errors: shielded / testnet / bad-checksum / not-taproot, each
   with the plain-language message). `--payout-script-hex` stays for
   compatibility; the two flags are mutually exclusive.
2. **Interactive mode** — when NO address is provided by flag or config AND
   stdin is a TTY: print the banner, prompt for the address, validate with
   the typed messages, re-prompt on error (3 attempts, then exit with help).
   Never prompt when stdin is not a TTY (scripts/services fail fast with a
   clear "no address configured" error instead of hanging).
3. **Config file** — `~/.config/dinero-miner/config.json` (platform config
   dir via the `dirs` crate): `{address, pool, server_pubkey, reward_mode,
   threads}`. Precedence: flags > config > built-in defaults. A successfully
   validated interactive address is written back ("saved for next time");
   next interactive run offers Enter-to-reuse:
   `mine to din1pgp5…maph9u? [Enter = yes / paste a new address]`.
4. **Built-in defaults** — pool `173.249.200.59:4444`, server pubkey
   `3c879d90c9bb430493dfbf02cecbb93c3ae0d9d6c31d0757595e353fbe927417`,
   reward_mode `shared`, threads = logical cores − 1 (min 1). Key-rotation
   escape hatch: config overrides the pinned pubkey, so a rotation is a
   one-line config edit or re-run of the install script — never a stranded
   binary (lesson: the 2026-07-09 SJ key rotation stranded the June pin).
5. **Human display mode** (default when stdout is a TTY) replaces the
   current `[event] {json}` lines:
   - one self-overwriting status line (`\r`): hashrate, ok/rej shares,
     blocks, uptime — repainted on each hashrate/share event;
   - block finds print the permanent banner block shown above (hash, nonce,
     thousands-separated tries, mode, local time) and then the status line
     resumes below;
   - connection lifecycle on single lines: `connecting…`, `connected ·
     pool … · shared rewards · N threads`, `pool unreachable — retrying in
     5s`;
   - Ctrl-C: stop cleanly, print a session summary line (duration, shares,
     blocks).
   `--json` and non-TTY stdout keep today's machine formats byte-for-byte —
   DineroMiner GUI and the fixture tests depend on them.
6. **GPU miner parity** — same flags/prompt/config/display; backend line adds
   `metal|opencl|cuda`.

## Distribution

- **Release matrix** (GitHub Releases on DineroLabs/dinero-sv2, tag
  `miner-vX.Y.Z` to stay clear of pool/protocol tags): `aarch64-apple-darwin`,
  `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu` (Chromebook/Crostini on Arm), CPU miner for
  all four; GPU miner where backends exist (macOS/Metal, Windows+Linux
  OpenCL; CUDA build if the toolchain lane allows). Static-link (musl for
  Linux) where practical. `SHA256SUMS` in every release, macOS binaries
  codesigned `Developer ID Application: DineroLabs LLC` + notarized per the
  DineroDPI pipeline.
- **Install one-liners** committed in-repo and served from the release:
  - `scripts/install.sh` (macOS/Linux/Crostini): detect OS/arch → download
    from the latest `miner-v*` release → verify SHA-256 → install to
    `~/.local/bin/dinero-miner` (+ PATH hint if needed).
  - `scripts/install.ps1` (Windows): same flow into
    `%LOCALAPPDATA%\DineroMiner\bin` + user PATH.
- **README section** with the paste-one-command install per OS and the
  three-line quickstart. Chromebook subsection: enable Linux (Crostini),
  paste the same Linux one-liner.

## Explicit non-goals

- No node-RPC solo mode in the CLI (that lives in DineroMiner's advanced
  panel for node operators).
- No auto-update, telemetry, or daemonization (systemd/launchd examples in
  the README are enough).
- No curses/TUI dashboard — one status line + banners is the whole display.

## Testing

- Unit: address validation/conversion vectors (reuse DineroMiner's, including
  the two real mainnet addresses), config precedence table
  (flag>config>default), interactive-prompt state machine over a scripted
  stdin (valid, invalid→retry→valid, 3-strikes), non-TTY-without-address
  fails fast.
- Golden-output: `--json` mode byte-compatibility against the captured
  DineroMiner fixtures (`sv2-miner-json.log`) — the GUI contract must not
  drift.
- Live: 60 s against the SJ pool in shared mode per release candidate
  (shares accepted), matching the DineroMiner verification recipe.
- Install scripts: CI matrix job runs each installer in a clean
  container/VM per platform and executes `dinero-miner --version`.

## Risks

- Windows build lane for the Noise/crypto deps is unproven (pure-Rust deps
  expected to just work; verify first in the plan).
- Pool capacity/abuse once installation is one paste — out of scope here,
  but the pool's vardiff + per-connection limits should be reviewed before
  the README goes wide.
