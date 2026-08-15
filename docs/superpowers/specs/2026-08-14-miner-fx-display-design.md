# Miner FX display: live hash feed + color for the human TTY mode (design)

Date: 2026-08-14
Status: approved by owner (brainstorm 2026-08-14, four decisions locked)
Origin: owner request after the `miner-v0.1.0` release — the plain terminal
display works but should *feel* alive ("younger people want to feel like
it's hacking") while staying honest and cheap.

## Owner decisions (locked)

1. **Intensity:** live hash stream + color on the existing single-screen
   flow. NOT a full TUI dashboard; NOT color-only polish. Zero new
   dependencies — raw ANSI escape codes.
2. **Default:** FX is the default whenever stdout is a TTY. `--plain`
   restores the v1 quiet display; the `NO_COLOR` env convention is
   respected (layout keeps, colors strip).
3. **Authenticity:** the scrolling feed shows REAL candidate hashes from
   the machine's current sweep — never decorative random hex.
4. **Scrollback:** the feed animates inside a fixed window (in-place
   repaint). Permanent scrollback contains only real events: banner,
   lifecycle lines, block banners, session summary.

## What the screen does

- **Startup:** DINERO block-letter ASCII banner (color), then the normal
  lifecycle lines (`connected · pool … · shared rewards · N threads`).
- **Live region** (repainted in place):
  - ~8-line **hash feed**: `0x<nonce>… <hash>… ✗` lines in dim green.
    Share submissions render bright green (`▓ SHARE ✓`), rejects red,
    stale-job notices yellow. Lines scroll within the window only.
  - **Status line** below the feed, bold:
    `⛏ 4.19 MH/s │ 14 ok │ 0 rej │ ▂▃▅▇ │ up 3m12s` — the sparkline
    renders the most recent 12 hashrate samples, one ▁▂▃▄▅▆▇█ cell each,
    scaled to the min/max of those samples. GPU miner appends the
    backend name (`· metal`).
- **Block found:** feed freezes, a gold full-width flash animation plays
  (~5 frames over ~1 s), then the permanent gold block banner (v1 format,
  colored) prints ABOVE the live region so it survives in scrollback.
  The live region resumes below.
- **Ctrl-C:** clears the live region, prints the session summary
  (existing v1 renderer, colored).

## How the real hashes work

The hashing hot paths do NOT stream hashes. They publish a **nonce
position hint**:

- CPU (rayon sweep): one `Relaxed` atomic store every ~262,144 hashes.
- GPU: the host-side `nonce_start` of the current batch (already known;
  stored to the same atomic).

A 10 Hz **display ticker thread** (spawned only in FX mode) reads the
hint plus the current job's assembled header bytes, computes ONE real
sha256d for that candidate nonce, and pushes the line into the feed.
Every displayed hash is a genuine candidate from the current sweep, at a
display cost of 10 hashes/second (~0.0002% of the work), with identical
treatment for CPU and GPU and zero hot-path branching.

## Where the code lives

- `dinero-miner-ux::display` (pure string functions, all unit-testable):
  - `theme` — ANSI color/style constants + `strip_ansi()` helper.
  - `banner()` — startup ASCII art.
  - `sparkline(&[f64]) -> String` — ▁▂▃▄▅▆▇█ from recent samples.
  - `feed_line(kind, nonce, hash, width) -> String` — one feed row
    (kinds: Candidate, Share, Reject, Stale).
  - `celebration_frames(width) -> Vec<String>` — the gold flash.
  - `FeedWindow` — holds the ring buffer of feed rows + stats; its
    `repaint(width) -> String` returns the full in-place redraw string
    (cursor-up codes included); `clear() -> String` removes the region.
- `dinero-miner-ux::fx` — the runtime: `FeedSource` (shared job header
  bytes + nonce-hint `AtomicU64` + target) and the 10 Hz ticker thread
  that computes the sampled hash and drives `FeedWindow`.
- Each miner's `main.rs`: the existing `emit_human` seam feeds events
  into `FeedWindow` instead of the v1 one-liner; job-change code updates
  `FeedSource`; the hot loops store the nonce hint. The v1 renderer
  stays as the `--plain` path.

## Fallbacks and contracts (unchanged guarantees)

- `--plain` → exactly the v1 human display (existing code path).
- `NO_COLOR` set → FX layout, colors stripped.
- No cursor support → automatic fallback to `--plain`. Detection rule:
  `TERM` unset, empty, or equal to `dumb`.
- Terminal narrower than 60 columns → feed lines truncate with `…`.
- `--json` and non-TTY plain output stay BYTE-IDENTICAL (the FX branch
  forks at the same seam human mode already does; the golden fixture
  test keeps guarding the GUI contract).

## Testing

- Unit: every renderer is a pure function — assert exact strings
  (including ANSI) plus `strip_ansi` content asserts; `sparkline`
  bucket math; `feed_line` truncation at narrow widths.
- `FeedWindow::repaint`/`clear` against a virtual terminal buffer:
  cursor math correct at multiple widths and fill levels.
- Integration: nonce hint advances while hashing runs; ticker produces
  hashes that verify against the job header.
- Golden: existing JSON fixture test unchanged and passing.
- Manual gate: `expect`-driven TTY run (as in the v0.1.0 release gate)
  plus an owner eyeball run.

## Non-goals

- No TUI framework/alternate screen buffer, no mouse, no panels.
- No decorative fake data anywhere.
- No new dependencies.
- No change to `--json`, non-TTY, or GUI sidecar behavior.
