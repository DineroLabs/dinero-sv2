# Miner FX Display Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the human TTY mode of both miners a live "hacking" display — real sampled hashes scrolling in a fixed window, color, sparkline, gold block celebration — per `docs/superpowers/specs/2026-08-14-miner-fx-display-design.md`.

**Architecture:** All renderers are pure string functions in `dinero-miner-ux` (`theme` + additions to `display`); a new `fx` module owns the runtime (`FxScreen` writer + 10 Hz ticker thread). The miners keep their existing `Emitter` seam: a new `OutputMode::Fx` routes the same events to `FxScreen`; the v1 human renderer becomes the `--plain` path, untouched.

**Tech Stack:** Rust, existing workspace only. NO new external dependencies (raw ANSI escapes; terminal width via `COLUMNS`/`tput`).

## Global Constraints

- Branch `feat/fx-display` (already exists, off main `3a9156e`). Commit after every task.
- FX is DEFAULT when stdout is a TTY; `--plain` restores v1 human display; `NO_COLOR` env strips colors but keeps FX layout; `TERM` unset/empty/`dumb` → automatic `--plain` fallback.
- `--json` and non-TTY plain output stay BYTE-IDENTICAL (existing fixture test `crates/dinero-sv2-miner/tests/json_compat.rs` must keep passing untouched).
- Status line spells out **"rejected"** — never "rej". Motto under banner, gold, exact text: `· Real Money For Free People ·`.
- Every displayed hash is a REAL candidate: sampled nonce hint + `HeaderAssembly::hash` recompute at 10 Hz. No decorative hex anywhere.
- Feed window height = 8 rows; the live region is CONSTANT at 10 lines (last-block panel + 8 feed rows + status); sparkline = most recent 12 hashrate samples, cells `▁▂▃▄▅▆▇█`, min/max scaled; width floor 60 columns (truncate rows with `…`).
- NO permanent per-block banners (owner call 2026-08-15): celebration flash plays below the status line, then only the in-region last-block panel + the status line's session DIN total change. Scrollback = banner, lifecycle lines, exit summary — nothing else, ever.
- Session DIN total: solo = exact coinbase value from `new_job`; shared = `100 DIN subsidy × window_bps/10_000` estimate, total prefixed `≈` once any estimate is included. Verify `UNA_PER_DIN` and the 100 DIN subsidy against dinero-v8 sources at implementation.
- Zero new dependencies in any `Cargo.toml`.

---

### Task 1: `theme` module — ANSI constants, strip, environment detection

**Files:**
- Create: `crates/dinero-miner-ux/src/theme.rs`
- Modify: `crates/dinero-miner-ux/src/lib.rs` (add `pub mod theme;`)

**Interfaces:**
- Produces:

```rust
pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM_GREEN: &str = "\x1b[2;32m";     // feed candidates
pub const BRIGHT_GREEN: &str = "\x1b[1;92m";  // shares / accents
pub const GOLD: &str = "\x1b[1;33m";          // blocks / motto
pub const RED: &str = "\x1b[1;31m";           // rejected
pub const YELLOW: &str = "\x1b[33m";          // stale notices
pub const FAINT: &str = "\x1b[2m";            // rules / separators

/// Wraps `s` in `code` + RESET when `colors`, else returns `s` verbatim.
pub fn paint(code: &str, s: &str, colors: bool) -> String;
/// Removes CSI escape sequences (a small state machine, no regex).
pub fn strip_ansi(s: &str) -> String;
/// NO_COLOR convention: colors are on iff the env var is unset.
pub fn colors_enabled() -> bool;
/// Spec detection rule: TERM unset, empty, or "dumb" → no FX.
pub fn term_supports_fx() -> bool;
/// COLUMNS env → `tput cols` (one spawn) → 100. Clamped to >= 60.
pub fn term_width() -> usize;
```

- [ ] **Step 1: Write the failing tests** (bottom of `theme.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paint_wraps_and_passes_through() {
        assert_eq!(paint(BRIGHT_GREEN, "hi", true), "\x1b[1;92mhi\x1b[0m");
        assert_eq!(paint(BRIGHT_GREEN, "hi", false), "hi");
    }
    #[test]
    fn strip_removes_csi_sequences() {
        assert_eq!(strip_ansi("\x1b[1;92mhi\x1b[0m there \x1b[5Fx"), "hi there x");
        assert_eq!(strip_ansi("plain"), "plain");
    }
    #[test]
    fn term_support_rule() {
        // helper with injected value so the test doesn't touch real env
        assert!(!term_ok(None));
        assert!(!term_ok(Some("")));
        assert!(!term_ok(Some("dumb")));
        assert!(term_ok(Some("xterm-256color")));
    }
    #[test]
    fn width_floor_is_60() {
        assert_eq!(width_from(Some("45".into())), 60);
        assert_eq!(width_from(Some("132".into())), 132);
        assert_eq!(width_from(Some("junk".into())), 100);
        assert_eq!(width_from(None), 100);
    }
}
```

- [ ] **Step 2:** `cargo test -p dinero-miner-ux theme` → FAIL (module missing).
- [ ] **Step 3: Implement.** `strip_ansi`: iterate chars; on `\x1b` followed by `[`, skip until a char in `@`..=`~` (inclusive), else emit. Structure env-reading fns as thin wrappers over pure helpers used by the tests: `fn term_ok(term: Option<&str>) -> bool`, `fn width_from(columns: Option<String>) -> usize` (parse → clamp `.max(60)`, parse-failure/None → 100). `term_width()` = `width_from(env COLUMNS)` but if COLUMNS unset, try `std::process::Command::new("tput").arg("cols")` output before falling back. `colors_enabled()` = `std::env::var_os("NO_COLOR").is_none()`.
- [ ] **Step 4:** `cargo test -p dinero-miner-ux theme` → 4 PASS.
- [ ] **Step 5:** Commit — `feat(fx): theme module — ANSI palette, strip_ansi, env detection`

---

### Task 2: banner + sparkline renderers

**Files:**
- Modify: `crates/dinero-miner-ux/src/display.rs` (append; existing v1 fns untouched)

**Interfaces:**
- Produces:

```rust
/// DINERO block-letter banner + gold motto line. Ends with '\n'.
pub fn banner(colors: bool) -> String;
/// Most recent 12 samples → one ▁▂▃▄▅▆▇█ cell each, min/max scaled.
/// Fewer than 12 samples: left-pad with ▁. Empty: 12 × ▁.
pub fn sparkline(samples: &[f64]) -> String;
```

- [ ] **Step 1: Write the failing tests** (append to display.rs tests mod)

```rust
#[test]
fn banner_carries_motto_and_art() {
    let plain = crate::theme::strip_ansi(&banner(true));
    assert!(plain.contains("██████╗"));
    assert!(plain.contains("· Real Money For Free People ·"));
    assert!(banner(true).contains("\x1b[1;33m"), "motto painted gold");
    assert!(!banner(false).contains('\x1b'), "no ANSI when colors off");
}
#[test]
fn sparkline_scales_min_max() {
    assert_eq!(sparkline(&[]), "▁▁▁▁▁▁▁▁▁▁▁▁");
    let s = sparkline(&[1.0, 8.0]);
    assert_eq!(s.chars().count(), 12);
    assert!(s.ends_with("▁█"), "min→▁, max→█, left-padded: got {s}");
    // flat input → mid cell, not divide-by-zero
    assert!(sparkline(&[4.0, 4.0, 4.0]).ends_with("▄▄▄"));
    // more than 12 samples → only the last 12 render
    let many: Vec<f64> = (0..30).map(|i| i as f64).collect();
    assert_eq!(sparkline(&many).chars().count(), 12);
}
```

- [ ] **Step 2:** Run → FAIL. **Step 3: Implement.** Banner art (exact, 6 lines, two leading spaces each — same face as the FX preview artifact):

```text
  ██████╗ ██╗███╗   ██╗███████╗██████╗  ██████╗
  ██╔══██╗██║████╗  ██║██╔════╝██╔══██╗██╔═══██╗
  ██║  ██║██║██╔██╗ ██║█████╗  ██████╔╝██║   ██║
  ██║  ██║██║██║╚██╗██║██╔══╝  ██╔══██╗██║   ██║
  ██████╔╝██║██║ ╚████║███████╗██║  ██║╚██████╔╝
  ╚═════╝ ╚═╝╚═╝  ╚═══╝╚══════╝╚═╝  ╚═╝ ╚═════╝
```

Art painted `theme::BRIGHT_GREEN`, motto line `        · Real Money For Free People ·` painted `theme::GOLD`. `sparkline`: take `samples[samples.len().saturating_sub(12)..]`; cell = `((v-min)/(max-min)*7).round()` indexing `['▁','▂','▃','▄','▅','▆','▇','█']`; `max==min` → index 3; left-pad with `▁` to 12.
- [ ] **Step 4:** Run → PASS. **Step 5:** Commit — `feat(fx): banner with motto + hashrate sparkline`

---

### Task 3: feed rows + celebration frames

**Files:**
- Modify: `crates/dinero-miner-ux/src/display.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, PartialEq)]
pub enum FeedKind { Candidate, Share, Rejected, Stale }
/// One feed row: `  0x<nonce8>  <hash-prefix>…  <suffix>`, truncated to
/// `width` display chars (ANSI excluded) ending with `…` when needed.
pub fn feed_line(kind: FeedKind, nonce: u32, hash: &[u8; 32], width: usize, colors: bool) -> String;
/// Gold flash frames: █×n, ▓×n, █×n, ▒×n, then "■■■  B L O C K   F O U N D   #<no>  ■■■".
pub fn celebration_frames(width: usize, block_no: u64, colors: bool) -> Vec<String>;
```

Suffixes: Candidate `✗` (faint), Share `▓ SHARE ✓ pool accepted` (bright green, whole row), Rejected `✗ rejected` (red, whole row), Stale `↻ stale job` (yellow, whole row). Hash renders as lowercase hex of the first 10 bytes + `…` (20 hex chars).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn feed_line_kinds_and_truncation() {
    let h = [0u8; 32];
    let cand = feed_line(FeedKind::Candidate, 0x8f31a2c4, &h, 100, false);
    assert_eq!(cand, "  0x8f31a2c4  00000000000000000000…  ✗");
    let share = feed_line(FeedKind::Share, 1, &h, 100, false);
    assert!(share.ends_with("▓ SHARE ✓ pool accepted"));
    assert!(feed_line(FeedKind::Rejected, 1, &h, 100, false).ends_with("✗ rejected"));
    assert!(feed_line(FeedKind::Stale, 1, &h, 100, false).ends_with("↻ stale job"));
    // width 60: full candidate row already fits (39 chars); width smaller
    // than content truncates with … at exactly `width` chars
    let narrow = feed_line(FeedKind::Share, 1, &h, 30, false);
    assert_eq!(narrow.chars().count(), 30);
    assert!(narrow.ends_with('…'));
    // colored share row carries bright green and resets
    let colored = feed_line(FeedKind::Share, 1, &h, 100, true);
    assert!(colored.contains("\x1b[1;92m") && colored.ends_with("\x1b[0m"));
}
#[test]
fn celebration_shape() {
    let f = celebration_frames(80, 3, false);
    assert_eq!(f.len(), 5);
    assert!(f[0].trim_start().chars().all(|c| c == '█'));
    assert!(f[4].contains("B L O C K   F O U N D") && f[4].contains("#3"));
    let colored = celebration_frames(80, 1, true);
    assert!(colored[0].contains("\x1b[1;33m"));
}
```

- [ ] **Step 2:** Run → FAIL. **Step 3: Implement.** Build the plain row first, truncate on `chars().count() > width` to `width-1` chars + `…`, THEN paint the whole row with the kind's color via `theme::paint` (so truncation math never sees ANSI). Flash bars: `"  " + ch.repeat(width.saturating_sub(24).min(56))`.
- [ ] **Step 4:** Run → PASS. **Step 5:** Commit — `feat(fx): feed rows + block celebration frames`

---

### Task 4: `FeedWindow` — fixed-region state + repaint/clear strings

**Files:**
- Modify: `crates/dinero-miner-ux/src/display.rs`

**Interfaces:**
- Consumes: `feed_line`, `sparkline`, `SessionStats`, `theme`.
- Produces:

```rust
pub const FEED_HEIGHT: usize = 8;
/// Region = 1 last-block panel line + FEED_HEIGHT feed rows + 1 status
/// line = 10 lines, CONSTANT from the first frame (panel renders blank
/// until the first find) so cursor math never varies. NO permanent
/// per-block banners exist (owner call 2026-08-15): the panel updates
/// in place and scrollback stays clean however many blocks land.
pub const REGION_LINES: usize = FEED_HEIGHT + 2;
pub struct FeedWindow {
    pub stats: SessionStats,      // reuses the v1 struct (ok/rej/blocks/hashrate/started)
    pub backend: Option<String>,  // GPU sets; appended to status line
    pub last_block: Option<String>,   // pre-rendered gold panel line
    pub session_din_una: u64,     // accumulated payout estimate, in una
    pub din_estimated: bool,      // true once any shared (≈) component added
    rows: std::collections::VecDeque<String>,  // pre-rendered rows
    rates: Vec<f64>,              // for the sparkline (cap 40)
    painted: bool,                // false until first repaint
}
impl FeedWindow {
    pub fn new() -> Self;
    pub fn push_row(&mut self, row: String);          // keeps last FEED_HEIGHT
    pub fn record_rate(&mut self, mhs: f64);          // pushes rate + stats.hashrate_hs
    /// Sets the panel + adds `value_una` to the session total.
    /// `estimated` marks the total with ≈ from then on.
    /// Panel format: `  ■ block #<n> · <local_time> · <hash16>…`
    pub fn record_block(&mut self, no: u64, hash: &str, local_time: &str, value_una: u64, estimated: bool);
    /// FX status line — "rejected" spelled out (spec); DIN token only
    /// after the first block:
    /// `  ⛏ 4.19 MH/s │ 14 ok │ 0 rejected │ blocks 2 │ ≈87.30 DIN │ ▂▃▅▇… │ up 3m12s[ · metal]`
    pub fn status_line_fx(&self, colors: bool) -> String;
    /// Full in-place redraw of the 10-line region. First call paints
    /// fresh; later calls prefix cursor-up. Every line ends with `\x1b[K`;
    /// lines are newline-separated; status is last, NO trailing newline.
    pub fn repaint(&mut self, width: usize, colors: bool) -> String;
    /// Erases the live region and resets `painted` (call before printing
    /// a permanent line, then repaint below it).
    pub fn clear(&mut self) -> String;
}
```

DIN formatting: `una / COIN` with two decimals; `COIN` is the chain's
una-per-DIN constant — the implementer MUST verify it against dinero-v8
(`src/consensus` amount constants; expected 100_000_000) and add
`pub const UNA_PER_DIN: u64` to `display.rs` with a comment citing the
verified source location.

Cursor math (pin in tests): non-first `repaint` prefixes `\x1b[10F`
(cursor to column 1, 10 lines up). `clear()` when painted: the cursor
rests on the status line (line 10 of the region), so `clear()` =
`"\r\x1b[K"` followed by `"\x1b[1A\x1b[K"` repeated 9 times, leaving the
cursor at column 1 of the first (now blank) panel row. When not painted,
both return no cursor codes (`repaint` just draws, `clear` returns `""`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn feed_window_repaint_and_clear_cursor_math() {
    let mut w = FeedWindow::new();
    w.push_row("  row-a".into());
    w.record_rate(4.19);
    let first = w.repaint(80, false);
    assert!(!first.starts_with("\x1b["), "first paint has no cursor-up");
    assert_eq!(first.matches('\n').count(), REGION_LINES - 1,
        "panel + 8 rows + status = 10 lines from the very first frame");
    assert!(first.contains("  row-a\x1b[K"));
    assert!(!first.ends_with('\n'));
    let second = w.repaint(80, false);
    assert!(second.starts_with("\x1b[10F"), "later paints move up 10 lines");
    let clear = w.clear();
    assert_eq!(clear, format!("\r\x1b[K{}", "\x1b[1A\x1b[K".repeat(9)));
    assert!(!w.clear().contains('\x1b'), "cleared window clears to nothing");
}
#[test]
fn status_line_fx_wording_and_din_total() {
    let mut w = FeedWindow::new();
    w.stats.ok = 14; w.stats.rej = 2;
    w.record_rate(4.19);
    let s = crate::theme::strip_ansi(&w.status_line_fx(true));
    assert!(s.contains("4.19 MH/s") && s.contains("14 ok"));
    assert!(s.contains("2 rejected"), "spec: never 'rej'");
    assert!(!s.contains(" rej "), "abbreviation banned");
    assert!(s.contains('│') && s.contains('▁'));
    assert!(!s.contains("DIN"), "no DIN token before the first block");
    w.backend = Some("metal".into());
    assert!(crate::theme::strip_ansi(&w.status_line_fx(false)).contains("· metal"));
}
#[test]
fn last_block_panel_and_session_din() {
    let mut w = FeedWindow::new();
    // solo: exact value (no ≈). 100 DIN = 100 × UNA_PER_DIN.
    w.record_block(1, "000000574714975b", "14:22:07", 100 * UNA_PER_DIN, false);
    let s = crate::theme::strip_ansi(&w.status_line_fx(false));
    assert!(s.contains("blocks 1") && s.contains("100.00 DIN") && !s.contains('≈'));
    let panel = crate::theme::strip_ansi(w.last_block.as_deref().unwrap());
    assert!(panel.contains("■ block #1") && panel.contains("14:22:07") && panel.contains("000000574714975b"));
    // shared: estimated 45% of 100 DIN → total flips to ≈
    w.record_block(2, "0000003a861a070d", "15:01:44", 45 * UNA_PER_DIN, true);
    let s2 = crate::theme::strip_ansi(&w.status_line_fx(false));
    assert!(s2.contains("blocks 2") && s2.contains("≈145.00 DIN"));
    assert!(crate::theme::strip_ansi(w.last_block.as_deref().unwrap()).contains("block #2"));
}
```

- [ ] **Step 2:** Run → FAIL. **Step 3: Implement.** `repaint`: pad `rows` up to `FEED_HEIGHT` with empty strings (blank lines) so the region height is constant from the first frame; each row line = `row + "\x1b[K"`, joined with `\n`, then `\n` + `status_line_fx` + `"\x1b[K"`. Uptime from `stats.started` like v1 `session_summary`. Rate: `record_rate` sets `stats.hashrate_hs = mhs * 1e6` and pushes to `rates` (truncate front at 40).
- [ ] **Step 4:** Run → PASS. **Step 5:** Commit — `feat(fx): FeedWindow fixed-region renderer`

---

### Task 5: `fx` runtime — `FxScreen` + 10 Hz sampler ticker

**Files:**
- Create: `crates/dinero-miner-ux/src/fx.rs`
- Modify: `crates/dinero-miner-ux/src/lib.rs` (add `pub mod fx;`)

**Interfaces:**
- Consumes: Task 2–4 renderers, `theme`.
- Produces:

```rust
pub struct CandidateSample { pub nonce: u32, pub hash: [u8; 32] }
/// Returns a REAL candidate from the miner's current sweep, or None
/// between jobs. Provided by each miner's main.rs.
pub type HashSampler = std::sync::Arc<dyn Fn() -> Option<CandidateSample> + Send + Sync>;

pub struct FxConfig {
    pub width: usize,
    pub colors: bool,
    pub reward_mode: String,     // block banner "mode" line
    pub frame_delay_ms: u64,     // celebration frame gap; 0 in tests
}
#[derive(Clone)]
pub struct FxScreen { /* Arc<Mutex<Inner>>: FeedWindow + Box<dyn Write+Send> + FxConfig */ }
impl FxScreen {
    pub fn new(out: Box<dyn std::io::Write + Send>, cfg: FxConfig) -> Self;
    pub fn print_banner(&self);                       // banner() + blank line
    pub fn lifecycle(&self, line: &str);              // clear → permanent line → repaint
    pub fn set_backend(&self, backend: &str);
    pub fn on_hashrate(&self, mhs: f64);              // record_rate → repaint
    pub fn on_share_ok(&self, n: u64);                // stats.ok += n; Share feed row uses last sample
    pub fn on_share_rejected(&self);                  // stats.rej += 1; Rejected row
    pub fn on_candidate(&self, s: CandidateSample);   // Candidate row → repaint
    /// Latest PPLNS window share, from `window_status` events (basis points).
    pub fn on_window(&self, bps: u64);
    /// Latest solo-template coinbase value, from solo `new_job` events.
    pub fn on_solo_job_value(&self, una: u64);
    /// Celebration (owner calls: bottom of screen, NO permanent banner).
    /// Freeze feed; write "\n" + frame + "\x1b[K", then overwrite that
    /// same line per frame with "\r" + frame + "\x1b[K" (sleep
    /// frame_delay_ms between frames); erase region + frame line
    /// (clear variant covering REGION_LINES + 1 lines); then
    /// `record_block` on the window and repaint — the last-block panel
    /// and the status DIN total are the only lasting trace. Value:
    /// solo → last `on_solo_job_value` (estimated=false);
    /// shared → `SHARED_BLOCK_SUBSIDY_UNA × window_bps / 10_000`
    /// (estimated=true; bps default 10_000 if no window seen).
    /// `pub const SHARED_BLOCK_SUBSIDY_UNA: u64 = 100 * UNA_PER_DIN;`
    /// — verify the 100 DIN subsidy against dinero-v8 at implementation
    /// and cite the source in a comment.
    pub fn on_block(&self, hash_hex: &str, local_time: &str);
    pub fn print_summary(&self);                      // clear region, v1 session_summary
    /// 10 Hz loop calling `tick` until `stop` is true. Detached std thread.
    pub fn spawn_ticker(&self, sampler: HashSampler, stop: std::sync::Arc<std::sync::atomic::AtomicBool>);
    pub fn tick(&self, sampler: &HashSampler);        // one sample → on_candidate; test seam
}
```

Concurrency contract: one `Mutex<Inner>` guards window + writer + config; every method locks, mutates, writes the returned repaint/permanent strings to the writer, flushes. `on_block` holds the lock for the whole celebration (ticker calls simply queue behind it; at 5 frames × `frame_delay_ms` that's ~1 s, acceptable). `on_share_ok`/`on_share_rejected` reuse the most recent candidate sample's nonce/hash for the event row (store `last_sample: Option<CandidateSample>` in Inner; if None, synthesize row text without nonce: `  ▓ SHARE ✓ pool accepted`).

- [ ] **Step 1: Write the failing tests** (writer = `Vec<u8>` behind a mutex clone; helper `screen_with_buffer() -> (FxScreen, Arc<Mutex<Vec<u8>>>)` using a small `struct SharedBuf(Arc<Mutex<Vec<u8>>>)` that implements `Write`)

```rust
#[test]
fn tick_feeds_real_sample_and_repaints() {
    let (fx, buf) = screen_with_buffer();
    let sampler: HashSampler = std::sync::Arc::new(|| Some(CandidateSample { nonce: 0xabcd0001, hash: [7u8; 32] }));
    fx.tick(&sampler);
    let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    let plain = crate::theme::strip_ansi(&out);
    assert!(plain.contains("0xabcd0001"));
    assert!(plain.contains("07070707"), "hash prefix rendered");
    assert!(plain.contains("MH/s"), "status line painted");
}
#[test]
fn block_flashes_then_updates_panel_no_permanent_banner() {
    let (fx, buf) = screen_with_buffer(); // frame_delay_ms: 0, reward_mode "shared"
    fx.on_window(4500); // 45% PPLNS window
    fx.on_block("000000574714975b", "14:22:07");
    let plain = crate::theme::strip_ansi(&String::from_utf8(buf.lock().unwrap().clone()).unwrap());
    assert!(plain.contains("B L O C K   F O U N D"), "flash frames played");
    assert!(plain.contains("■ block #1") && plain.contains("14:22:07"), "panel updated");
    assert!(plain.contains("≈45.00 DIN"), "shared estimate = 45% of 100 DIN subsidy");
    assert!(!plain.contains("tries"), "no permanent v1 banner in FX mode");
    // flash precedes the panel repaint in the stream
    let flash = plain.find("B L O C K   F O U N D").unwrap();
    let panel = plain.find("■ block #1").unwrap();
    assert!(flash < panel, "celebration at the bottom, then panel update");
}
#[test]
fn share_and_reject_update_stats_rows() {
    let (fx, buf) = screen_with_buffer();
    fx.on_share_ok(3);
    fx.on_share_rejected();
    let plain = crate::theme::strip_ansi(&String::from_utf8(buf.lock().unwrap().clone()).unwrap());
    assert!(plain.contains("3 ok") && plain.contains("1 rejected"));
    assert!(plain.contains("SHARE ✓") && plain.contains("✗ rejected"));
}
#[test]
fn ticker_thread_stops_on_flag() {
    let (fx, _buf) = screen_with_buffer();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let n2 = n.clone();
    let sampler: HashSampler = std::sync::Arc::new(move || { n2.fetch_add(1, std::sync::atomic::Ordering::Relaxed); None });
    fx.spawn_ticker(sampler, stop.clone());
    std::thread::sleep(std::time::Duration::from_millis(350));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let calls = n.load(std::sync::atomic::Ordering::Relaxed);
    assert!(calls >= 2, "ticker ran ({calls} calls)");
    let at_stop = calls;
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(n.load(std::sync::atomic::Ordering::Relaxed) <= at_stop + 1, "stopped");
}
```

- [ ] **Step 2:** Run → FAIL. **Step 3: Implement.** `tick`: `sampler()` → `Some` → `on_candidate` (push `feed_line(Candidate, …)` + repaint); `None` → still repaint every 5th tick so uptime advances. `on_block` computes the value per the interface doc (solo exact / shared `SHARED_BLOCK_SUBSIDY_UNA × bps / 10_000` with `estimated=true`), calls `FeedWindow::record_block`, and repaints — no permanent output. Ticker loop: `while !stop { tick(); sleep(100ms) }`.
- [ ] **Step 4:** Run → PASS. **Step 5:** Commit — `feat(fx): FxScreen runtime + 10 Hz real-hash ticker`

---

### Task 6: wire FX into `dinero-sv2-miner` (CPU)

**Files:**
- Modify: `crates/dinero-sv2-miner/src/main.rs`

**Interfaces:**
- Consumes: `fx::{FxScreen, FxConfig, CandidateSample, HashSampler}`, `theme::{colors_enabled, term_supports_fx, term_width}`.
- Produces (internal): `Args.plain: bool`; `OutputMode::Fx(FxScreen)`; `SamplerState` shared with the sweep.

- [ ] **Step 1: Write the failing tests** (in `args_tests`)

```rust
#[test]
fn plain_flag_parses() {
    assert!(Args::try_parse_from(["m", "--plain"]).unwrap().plain);
    assert!(!Args::try_parse_from(["m"]).unwrap().plain);
}
#[test]
fn mode_selection_rules() {
    // pure helper: (json, tty, plain, term_ok) -> Mode
    use ModeChoice::*;
    assert_eq!(choose_mode(true,  true,  false, true),  Json);
    assert_eq!(choose_mode(false, false, false, true),  PlainMachine);
    assert_eq!(choose_mode(false, true,  true,  true),  HumanV1);
    assert_eq!(choose_mode(false, true,  false, false), HumanV1);
    assert_eq!(choose_mode(false, true,  false, true),  Fx);
}
```

- [ ] **Step 2:** Run → FAIL. **Step 3: Implement — Args + mode.** Add to `Args`:

```rust
/// Quiet human display (the pre-FX single status line). FX (live hash
/// feed + color) is the default on interactive terminals.
#[arg(long)]
plain: bool,
```

Add `enum ModeChoice { Json, PlainMachine, HumanV1, Fx }` + pure `fn choose_mode(json: bool, tty: bool, plain: bool, term_ok: bool) -> ModeChoice` (table above). In `async_main`, replace the `let human = …` line: compute `mode = choose_mode(args.json, stdout().is_terminal(), args.plain, theme::term_supports_fx())`; `human = matches!(mode, HumanV1)` keeps feeding the existing prompt code path (FX mode passes `human=true` to `prompt_for_new_address` too — the prompt look is shared).

- [ ] **Step 4: Implement — Emitter Fx arm.** `OutputMode` gains `Fx(FxScreen)`; `Emitter::new` takes the mode. In `Emitter::emit`, the Fx arm routes:

```rust
OutputMode::Fx(fx) => match event {
    "hashrate" => { if let Some(mhs) = data.get("mhs").and_then(|v| v.as_f64()) { fx.on_hashrate(mhs); } }
    "share_accepted" => fx.on_share_ok(data.get("accepted_count").and_then(|v| v.as_u64()).unwrap_or(1)),
    "share_rejected" => fx.on_share_rejected(),
    "window_status" => { if let Some(bps) = data.get("window_bps").and_then(|v| v.as_u64()) { fx.on_window(bps); } }
    "new_job" => {
        // Solo templates carry the exact coinbase value for the DIN total.
        if let Some(una) = data.get("coinbase_value_una").and_then(|v| v.as_u64()) { fx.on_solo_job_value(una); }
        // deliberately NOT surfaced as a lifecycle line — job churn would
        // spam the permanent history; the feed itself shows the work.
    }
    "set_new_prev_hash" => { /* same: silent in FX mode */ }
    "share_submitted" => {
        if data.get("meets_block_target").and_then(|v| v.as_bool()).unwrap_or(false) {
            fx.on_block(data.get("hash").and_then(|v| v.as_str()).unwrap_or(""), &now_hms());
        }
    }
    _ => fx.lifecycle(&lifecycle_line(event, data)),
},
```

`spawn_ctrlc_summary_handler` Fx arm: on Ctrl-C call `fx.print_summary()` then exit 0. After constructing the emitter in FX mode: `fx.print_banner()` before `emit_startup`.

- [ ] **Step 5: Implement — real-hash sampler.** Add near the hashing section:

```rust
/// Job snapshot + live nonce position for the FX display sampler.
struct SamplerState {
    job: std::sync::Mutex<Option<NewTemplateDinero>>,
    nonce_hint: AtomicU64,   // low 32 bits nonce; high 32 bits timestamp offset
}
```

Create one `Arc<SamplerState>` in `async_main`; pass it into `start_hashing` / `start_hashing_shared` / `start_hashing_template` (new parameter). At the top of `start_hashing_template`, store the job: `*sampler.job.lock().unwrap() = Some(tmpl.clone());`. In the rayon inner loop (right after `tries += 1;` in `ranges.par_iter().for_each`), add:

```rust
if tries & 0x3FFFF == 0 {
    sampler.nonce_hint.store(
        ((current_timestamp - tmpl_timestamp) << 32) | nonce as u64,
        Ordering::Relaxed,
    );
}
```

Build the `HashSampler` closure in `async_main` (FX mode only):

```rust
let sampler_state2 = Arc::clone(&sampler_state);
let sampler: dinero_miner_ux::fx::HashSampler = Arc::new(move || {
    let job = sampler_state2.job.lock().ok()?.clone()?;
    let hint = sampler_state2.nonce_hint.load(Ordering::Relaxed);
    let share = SubmitSharesDinero {
        channel_id: 0, sequence_number: 0, job_id: 0,
        nonce: hint as u32,
        timestamp: job.timestamp + (hint >> 32),
        version: job.version,
    };
    Some(dinero_miner_ux::fx::CandidateSample { nonce: hint as u32, hash: HeaderAssembly::hash(&job, &share) })
});
fx.spawn_ticker(sampler, stop_flag.clone());
```

(`stop_flag: Arc<AtomicBool>` created beside it; set `true` on session end — reconnects reuse the same screen/ticker, so create both once before the reconnect loop and never stop until process exit.)

- [ ] **Step 6:** `cargo test -p dinero-sv2-miner` → all PASS (incl. untouched `json_compat`). `cargo run --release -p dinero-sv2-miner -- --help` shows `--plain`.
- [ ] **Step 7: Manual TTY sanity** (expect, as in the v0.1.0 release gate):

```
spawn target/release/dinero-sv2-miner --no-save --threads 2 --address din1pafzgzwwfeqkfh7u4kkpe8qy97gey3zcvymx5eumxzx45m08q6tgqedz700
expect "Real Money For Free People"   ;# banner + motto
expect -re {0x[0-9a-f]{8}}            ;# live feed rows
expect "rejected"                     ;# status line wording (0 rejected)
```

Also verify `--plain` still renders the v1 single line, and `NO_COLOR=1` output contains no `\x1b[` color codes (cursor codes allowed).
- [ ] **Step 8:** Commit — `feat(miner): FX display default on TTY — live hash feed, banner, celebration`

---

### Task 7: wire FX into `dinero-sv2-gpu-miner`

**Files:**
- Modify: `crates/dinero-sv2-gpu-miner/src/main.rs`

**Interfaces:**
- Consumes: identical to Task 6.

- [ ] **Step 1:** Copy Task 6's two tests (`plain_flag_parses`, `mode_selection_rules`) into the GPU crate's tests. Run → FAIL.
- [ ] **Step 2: Implement.** Same `--plain` arg, `ModeChoice`/`choose_mode`, Emitter Fx arm (plus `"gpu_ready"` → `fx.set_backend(backend)` then `fx.lifecycle(...)`), banner print, Ctrl-C summary. Sampler: same `SamplerState` shape; the job is stored at the top of `start_hashing_gpu_template`; the nonce hint is stored host-side in the dispatch loop (in the batch loop, next to the existing `hashes_since_emit` accounting): `sampler.nonce_hint.store(((current_timestamp - tmpl_initial_timestamp) << 32) | nonce_start, Ordering::Relaxed);`. The sampler closure is identical to Task 6's (the GPU crate has the same `HeaderAssembly::hash`/`SubmitSharesDinero` imports). The `emit_gpu_hashrate` human arm changes from `emit_human(...)` to the Fx route (`fx.on_hashrate(mhs)`; keep the raw println for Json/Plain untouched). The v1 `HumanState` path stays for `--plain`.
- [ ] **Step 3:** `cargo test -p dinero-sv2-gpu-miner` → PASS. `--help` shows `--plain`.
- [ ] **Step 4:** Manual: `timeout 20` expect run as Task 6 Step 7 (backend name `metal` must appear in the status line).
- [ ] **Step 5:** Commit — `feat(gpu-miner): FX display parity (backend in status line)`

---

### Task 8: full verification + branch wrap-up

**Files:** none (fix regressions where found)

- [ ] **Step 1:** `cargo test --workspace` → every suite green; confirm `json_compat` fixture test ran and passed (grep the output for `json_compat`).
- [ ] **Step 2:** Byte-compat spot check: `timeout 20 cargo run --release -p dinero-sv2-miner -- --address din1pafzgz… --no-save --threads 2 --json | head -5` — event/key sets unchanged (compare to Task 8 of the v1 plan: startup/connected/channel_open shapes).
- [ ] **Step 3:** Live 60 s FX run against the pool (expect script): banner + motto once, feed scrolling, `N ok` incrementing, no scrollback growth beyond permanent lines (capture full transcript, assert the DINERO art appears exactly once).
- [ ] **Step 4:** Fallback matrix: `--plain` (v1 line), `NO_COLOR=1` (layout, no color codes), `TERM=dumb` (auto v1), `COLUMNS=60` (rows truncate, no wrap).
- [ ] **Step 5:** Commit any fixes — `test(fx): verification matrix` — push branch, open PR to main (owner merges; release ships with the next `miner-vX.Y.Z` tag).

---

## Self-Review (performed at write time)

- **Spec coverage:** banner+motto (T2), real-hash feed via hint+recompute at 10 Hz (T5/T6/T7), fixed 8-row window + clean scrollback (T4/T5), colors incl. NO_COLOR (T1, painted at render sites), sparkline 12 samples (T2), "rejected" wording (T4 test), celebration + permanent gold banner (T3/T5), default-on-TTY with `--plain`/TERM fallback (T6/T7 `choose_mode`), width floor 60 + truncation (T1/T3), GPU backend suffix (T4/T7), Ctrl-C summary (T5/T6), byte-identical JSON/non-TTY (untouched arms + T8 checks), zero new deps (all tasks), testing section (unit in T1–T5, integration/manual in T6–T8).
- **Placeholder scan:** none — every step carries code or exact commands. The banner art, cursor strings, and sampler bit-packing are spelled out.
- **Type consistency:** `FeedKind`/`feed_line` (T3) match `FeedWindow.push_row(String)` usage (T4/T5 pre-render rows); `CandidateSample`/`HashSampler` identical in T5/T6/T7; `choose_mode` signature identical in T6/T7; `frame_delay_ms: 0` used by T5 tests matches `FxConfig`.
