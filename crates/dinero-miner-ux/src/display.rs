use std::time::Instant;
use std::collections::VecDeque;

pub const FEED_HEIGHT: usize = 8;
/// Region = 1 last-block panel line + FEED_HEIGHT feed rows + 1 status
/// line = 10 lines, CONSTANT from the first frame (panel renders blank
/// until the first find) so cursor math never varies. NO permanent
/// per-block banners exist (owner call 2026-08-15): the panel updates
/// in place and scrollback stays clean however many blocks land.
pub const REGION_LINES: usize = FEED_HEIGHT + 2;

/// Verified from dinero-v8/src/consensus/consensus.hpp:
/// `static constexpr int64_t COIN = 100'000'000;  // 1 DIN = 100,000,000 units (8 decimals)`
pub const UNA_PER_DIN: u64 = 100_000_000;

/// DINERO block-letter banner + gold motto line. Ends with '\n'.
pub fn banner(colors: bool) -> String {
    let art_lines = [
        "  ██████╗ ██╗███╗   ██╗███████╗██████╗  ██████╗",
        "  ██╔══██╗██║████╗  ██║██╔════╝██╔══██╗██╔═══██╗",
        "  ██║  ██║██║██╔██╗ ██║█████╗  ██████╔╝██║   ██║",
        "  ██║  ██║██║██║╚██╗██║██╔══╝  ██╔══██╗██║   ██║",
        "  ██████╔╝██║██║ ╚████║███████╗██║  ██║╚██████╔╝",
        "  ╚═════╝ ╚═╝╚═╝  ╚═══╝╚══════╝╚═╝  ╚═╝ ╚═════╝",
    ];

    let mut result = String::new();
    for line in &art_lines {
        result.push_str(&crate::theme::paint(crate::theme::BRIGHT_GREEN, line, colors));
        result.push('\n');
    }

    let motto = "        · Real Money For Free People ·";
    result.push_str(&crate::theme::paint(crate::theme::GOLD, motto, colors));
    result.push('\n');

    result
}

/// Most recent 12 samples → one ▁▂▃▄▅▆▇█ cell each, min/max scaled.
/// Fewer than 12 samples: left-pad with ▁. Empty: 12 × ▁.
pub fn sparkline(samples: &[f64]) -> String {
    const SPARKLINE_CHARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const TARGET_LEN: usize = 12;

    // Take only the last 12 samples
    let active_samples = if samples.len() > TARGET_LEN {
        &samples[samples.len() - TARGET_LEN..]
    } else {
        samples
    };

    // If no samples, return 12 × ▁
    if active_samples.is_empty() {
        return "▁".repeat(TARGET_LEN);
    }

    // Find min and max
    let min = active_samples
        .iter()
        .fold(f64::INFINITY, |a, &b| a.min(b));
    let max = active_samples
        .iter()
        .fold(f64::NEG_INFINITY, |a, &b| a.max(b));

    // Map samples to sparkline characters
    let mut result = String::new();
    for &sample in active_samples {
        let index = if (max - min).abs() < 1e-10 {
            // min == max, use middle character
            3
        } else {
            let normalized = (sample - min) / (max - min);
            let scaled = (normalized * 7.0).round() as usize;
            scaled.min(7) // Clamp to valid range
        };
        result.push(SPARKLINE_CHARS[index]);
    }

    // Left-pad with ▁ to reach TARGET_LEN (count chars, not bytes)
    let char_count = result.chars().count();
    if char_count < TARGET_LEN {
        let padding_needed = TARGET_LEN - char_count;
        let mut padded = "▁".repeat(padding_needed);
        padded.push_str(&result);
        padded
    } else {
        result
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum FeedKind {
    Candidate,
    Share,
    Rejected,
    Stale,
}

/// One feed row: `  0x<nonce8>  <hash-prefix>…  <suffix>`, truncated to
/// `width` display chars (ANSI excluded) ending with `…` when needed.
pub fn feed_line(kind: FeedKind, nonce: u32, hash: &[u8; 32], width: usize, colors: bool) -> String {
    // Hash prefix: first 10 bytes = 20 hex chars
    let hash_hex = format!("{:02x?}", &hash[..10])
        .replace('[', "")
        .replace(']', "")
        .replace(", ", "")
        .to_lowercase();

    let suffix = match kind {
        FeedKind::Candidate => "✗".to_string(),
        FeedKind::Share => "▓ SHARE ✓ pool accepted".to_string(),
        FeedKind::Rejected => "✗ rejected".to_string(),
        FeedKind::Stale => "↻ stale job".to_string(),
    };

    // Build plain row: "  0x<nonce>  <hash>…  <suffix>"
    let plain_row = format!("  0x{:08x}  {}…  {}", nonce, hash_hex, suffix);

    // Truncate to width if needed
    let truncated = if plain_row.chars().count() > width {
        let truncated_str: String = plain_row.chars().take(width - 1).collect();
        format!("{}…", truncated_str)
    } else {
        plain_row
    };

    // Paint the row based on kind
    match kind {
        FeedKind::Candidate => crate::theme::paint(crate::theme::DIM_GREEN, &truncated, colors),
        FeedKind::Share => crate::theme::paint(crate::theme::BRIGHT_GREEN, &truncated, colors),
        FeedKind::Rejected => crate::theme::paint(crate::theme::RED, &truncated, colors),
        FeedKind::Stale => crate::theme::paint(crate::theme::YELLOW, &truncated, colors),
    }
}

/// Gold flash frames: █×n, ▓×n, █×n, ▒×n, then "■■■  B L O C K   F O U N D   #<no>  ■■■".
pub fn celebration_frames(width: usize, block_no: u64, colors: bool) -> Vec<String> {
    let mut frames = Vec::new();

    // Flash bar formula: "  " + ch.repeat(width.saturating_sub(24).min(56))
    let bar_len = width.saturating_sub(24).min(56);
    let bar = format!("  {}", "█".repeat(bar_len));

    // Frame 1: █ characters
    frames.push(crate::theme::paint(crate::theme::GOLD, &bar, colors));

    // Frame 2: ▓ characters
    let bar_mid = format!("  {}", "▓".repeat(bar_len));
    frames.push(crate::theme::paint(crate::theme::GOLD, &bar_mid, colors));

    // Frame 3: █ characters again
    frames.push(crate::theme::paint(crate::theme::GOLD, &bar, colors));

    // Frame 4: ▒ characters
    let bar_light = format!("  {}", "▒".repeat(bar_len));
    frames.push(crate::theme::paint(crate::theme::GOLD, &bar_light, colors));

    // Frame 5: Message
    let message = format!("■■■  B L O C K   F O U N D   #{}  ■■■", block_no);
    frames.push(crate::theme::paint(crate::theme::GOLD, &message, colors));

    frames
}

#[derive(Default)]
pub struct SessionStats {
    pub hashrate_hs: f64,
    pub ok: u64,
    pub rej: u64,
    pub blocks: u64,
    pub started: Option<Instant>,
}

pub struct Display;

impl Display {
    /// The one self-overwriting line. Caller prints with `\r{}` + flush.
    pub fn status_line(s: &SessionStats) -> String {
        format!(
            "⛏  {}  {} ok  {} rej  blocks {}",
            Self::fmt_hashrate(s.hashrate_hs),
            s.ok,
            s.rej,
            s.blocks
        )
    }

    /// Permanent block banner (spec format). `local_time` injected for tests.
    pub fn block_banner(no: u64, hash: &str, nonce: &str, tries: u64, mode: &str, local_time: &str) -> String {
        format!(
            "\n■ block found  #{}  {}\n  hash   {}\n  nonce  {}\n  tries  {}\n  mode   {}\n",
            no,
            local_time,
            hash,
            nonce,
            Self::group_thousands(tries),
            mode
        )
    }

    pub fn session_summary(s: &SessionStats, elapsed_secs: u64) -> String {
        let elapsed = Self::format_duration(elapsed_secs);
        format!(
            "Session: {} elapsed | {} ok | {} rej | blocks {}",
            elapsed,
            s.ok,
            s.rej,
            s.blocks
        )
    }

    pub fn fmt_hashrate(hs: f64) -> String {
        if hs < 999.995 {
            format!("{} H/s", hs as u64)
        } else if hs < 999_995.0 {
            format!("{:.2} kH/s", hs / 1e3)
        } else if hs < 999_999_500.0 {
            format!("{:.2} MH/s", hs / 1e6)
        } else {
            format!("{:.2} GH/s", hs / 1e9)
        }
    }

    pub fn group_thousands(n: u64) -> String {
        let s = n.to_string();
        if s.len() <= 3 {
            return s;
        }

        let mut result = String::new();
        for (i, c) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                result.insert(0, ',');
            }
            result.insert(0, c);
        }
        result
    }

    /// Helper to format elapsed time as XmYYs or XhYYmYYs
    fn format_duration(secs: u64) -> String {
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            let m = secs / 60;
            let s = secs % 60;
            format!("{}m{:02}s", m, s)
        } else {
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            let s = secs % 60;
            format!("{}h{:02}m{:02}s", h, m, s)
        }
    }
}

pub struct FeedWindow {
    pub stats: SessionStats,
    pub backend: Option<String>,
    pub last_block: Option<String>,
    pub session_din_una: u64,
    pub din_estimated: bool,
    rows: VecDeque<String>,
    rates: Vec<f64>,
    painted: bool,
}

impl FeedWindow {
    pub fn new() -> Self {
        FeedWindow {
            stats: SessionStats::default(),
            backend: None,
            last_block: None,
            session_din_una: 0,
            din_estimated: false,
            rows: VecDeque::new(),
            rates: Vec::new(),
            painted: false,
        }
    }

    pub fn push_row(&mut self, row: String) {
        self.rows.push_back(row);
        if self.rows.len() > FEED_HEIGHT {
            self.rows.pop_front();
        }
    }

    pub fn record_rate(&mut self, mhs: f64) {
        self.stats.hashrate_hs = mhs * 1e6;
        self.rates.push(mhs);
        if self.rates.len() > 40 {
            self.rates.remove(0);
        }
    }

    pub fn record_block(&mut self, no: u64, hash: &str, local_time: &str, value_una: u64, estimated: bool) {
        self.stats.blocks += 1;
        self.session_din_una += value_una;
        if estimated {
            self.din_estimated = true;
        }
        // Panel format: `  ■ block #<n> · <local_time> · <hash16>…`
        self.last_block = Some(format!("  ■ block #{} · {} · {}…", no, local_time, hash));
    }

    pub fn status_line_fx(&self, width: usize, colors: bool) -> String {
        let hashrate_str = Display::fmt_hashrate(self.stats.hashrate_hs);
        let sparkline_str = sparkline(&self.rates);

        // Format uptime
        let uptime_str = if let Some(started) = self.stats.started {
            let elapsed_secs = started.elapsed().as_secs();
            Display::format_duration(elapsed_secs)
        } else {
            "0s".to_string()
        };

        // Build status line with DIN token only if blocks > 0
        let mut status = format!(
            "  ⛏ {} │ {} ok │ {} rejected │ blocks {}",
            hashrate_str, self.stats.ok, self.stats.rej, self.stats.blocks
        );

        if self.stats.blocks > 0 {
            let din_total = self.session_din_una as f64 / UNA_PER_DIN as f64;
            let din_prefix = if self.din_estimated { "≈" } else { "" };
            status.push_str(&format!(" │ {}{:.2} DIN", din_prefix, din_total));
        }

        status.push_str(&format!(" │ {} │ up {}", sparkline_str, uptime_str));

        if let Some(ref backend) = self.backend {
            status.push_str(&format!(" · {}", backend));
        }

        // Truncate to width, same shape as feed_line: the status line shares
        // the fixed 10-line cursor-up-N region with the feed rows, so any
        // line wider than the real terminal wraps onto an extra screen row
        // and desyncs the `\x1b[NF` cursor math on the next repaint.
        let status = if status.chars().count() > width {
            let truncated: String = status.chars().take(width.saturating_sub(1)).collect();
            format!("{}…", truncated)
        } else {
            status
        };

        crate::theme::paint(crate::theme::BRIGHT_GREEN, &status, colors)
    }

    pub fn repaint(&mut self, width: usize, colors: bool) -> String {
        let mut output = String::new();

        // Non-first repaint: cursor up 9 lines to column 1 (a 10-row region
        // needs 9 up-moves to return from its last row to its first — see
        // clear()'s matching 9-up-move math below; `\x1b[10F` was an
        // off-by-one that made the region drift 1 row further up the screen
        // on every repaint), then erase cursor-to-end-of-screen so any
        // stale/duplicated lines left below the region by a desync from a
        // different cause (e.g. a lifecycle println! interleaved between
        // two repaints, or a taller-than-expected celebration frame)
        // self-heal within one redraw instead of accumulating.
        if self.painted {
            output.push_str("\x1b[9F\x1b[J");
        }

        // Panel line (blank if no last_block yet). `last_block` carries the
        // full 64-hex-char block hash (see record_block), so it must be
        // truncated to width the same way feed_line/status_line_fx are —
        // untruncated it's ~89 chars, which wraps on any terminal narrower
        // than ~91 columns, defeating the fixed 10-physical-row region and
        // the `\x1b[9F` cursor math above. `last_block` is stored as plain
        // text (painted only here at repaint time), so truncate before
        // painting — painting after truncation would count ANSI bytes
        // toward the width budget.
        if let Some(ref block) = self.last_block {
            let truncated = if block.chars().count() > width {
                let head: String = block.chars().take(width.saturating_sub(1)).collect();
                format!("{}…", head)
            } else {
                block.clone()
            };
            output.push_str(&crate::theme::paint(crate::theme::GOLD, &truncated, colors));
        } else {
            output.push(' ');
        }
        output.push_str("\x1b[K\n");

        // Feed rows: pad to FEED_HEIGHT with blanks
        for i in 0..FEED_HEIGHT {
            if i < self.rows.len() {
                output.push_str(&self.rows[i]);
            }
            output.push_str("\x1b[K\n");
        }

        // Status line (no trailing newline)
        output.push_str(&self.status_line_fx(width, colors));
        output.push_str("\x1b[K");

        self.painted = true;
        output
    }

    pub fn clear(&mut self) -> String {
        if !self.painted {
            return String::new();
        }

        let mut output = String::from("\r\x1b[K");
        for _ in 0..9 {
            output.push_str("\x1b[1A\x1b[K");
        }

        self.painted = false;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashrate_units() {
        assert_eq!(Display::fmt_hashrate(42.0), "42 H/s");
        assert_eq!(Display::fmt_hashrate(845_200.0), "845.20 kH/s");
        assert_eq!(Display::fmt_hashrate(2_090_000.0), "2.09 MH/s");
        assert_eq!(Display::fmt_hashrate(1_200_000_000.0), "1.20 GH/s");
    }

    #[test]
    fn thousands() {
        assert_eq!(Display::group_thousands(21_700_970), "21,700,970");
        assert_eq!(Display::group_thousands(950), "950");
    }

    #[test]
    fn status_line_contents() {
        let s = SessionStats {
            hashrate_hs: 2_090_000.0,
            ok: 14,
            rej: 1,
            blocks: 2,
            started: None,
        };
        let l = Display::status_line(&s);
        assert!(l.contains("2.09 MH/s") && l.contains("14 ok") && l.contains("1 rej") && l.contains("blocks 2"));
        assert!(!l.contains('\n'), "single line, caller uses \\r");
    }

    #[test]
    fn block_banner_format() {
        let b = Display::block_banner(1, "000000574714975b", "0x014b216a", 21_700_970, "shared", "14:22:07");
        let want = "\n■ block found  #1  14:22:07\n  hash   000000574714975b\n  nonce  0x014b216a\n  tries  21,700,970\n  mode   shared\n";
        assert_eq!(b, want);
    }

    #[test]
    fn summary_line() {
        let s = SessionStats {
            hashrate_hs: 0.0,
            ok: 30,
            rej: 2,
            blocks: 1,
            started: None,
        };
        let l = Display::session_summary(&s, 352);
        assert!(l.contains("5m52s") && l.contains("30") && l.contains("blocks 1"));
    }

    #[test]
    fn hashrate_units_never_show_mantissa_1000() {
        assert_eq!(Display::fmt_hashrate(999_999.0), "1.00 MH/s");
        assert_eq!(Display::fmt_hashrate(999_999_900.0), "1.00 GH/s");
    }

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
        // A 10-row region needs 9 total up-moves to return from the last row
        // (status line) to the first (panel line) — matching clear()'s own
        // 9-up-move math below. `\x1b[10F` (one too many) was the actual bug:
        // it makes the region drift 1 row further up the screen on every
        // single repaint, indistinguishable from correct on a screen where
        // the region happens to start at absolute row 0 (nowhere higher to
        // go), but catastrophic once anything (banner/lifecycle text) is
        // printed above it, since the region walks up through — and erases
        // — that content over time. `\x1b[J` (erase cursor-to-end-of-screen)
        // is added as defense in depth: it self-heals any stale/duplicated
        // lines left below the region by a desync from a different cause
        // (e.g. a lifecycle println! interleaved between two repaints).
        assert!(
            second.starts_with("\x1b[9F\x1b[J"),
            "later paints move up 9 lines (not 10) then erase to end of screen: {:?}",
            &second[..second.len().min(20)]
        );
        let clear = w.clear();
        assert_eq!(clear, format!("\r\x1b[K{}", "\x1b[1A\x1b[K".repeat(9)));
        assert!(!w.clear().contains('\x1b'), "cleared window clears to nothing");
    }
    #[test]
    fn status_line_fx_wording_and_din_total() {
        let mut w = FeedWindow::new();
        w.stats.ok = 14; w.stats.rej = 2;
        w.record_rate(4.19);
        let s = crate::theme::strip_ansi(&w.status_line_fx(200, true));
        assert!(s.contains("4.19 MH/s") && s.contains("14 ok"));
        assert!(s.contains("2 rejected"), "spec: never 'rej'");
        assert!(!s.contains(" rej "), "abbreviation banned");
        assert!(s.contains('│') && s.contains('▁'));
        assert!(!s.contains("DIN"), "no DIN token before the first block");
        w.backend = Some("metal".into());
        let with_backend = crate::theme::strip_ansi(&w.status_line_fx(200, false));
        assert!(with_backend.contains("· metal"));
        assert!(
            !with_backend.contains('['),
            "backend suffix must not be wrapped in literal brackets: {with_backend:?}"
        );
    }
    #[test]
    fn status_line_fx_truncates_to_width() {
        // Same failure mode as feed rows: the status line shares the fixed
        // 10-line cursor-up-N region, so an untruncated line wider than the
        // real terminal wraps onto an extra row and desyncs the repaint math
        // on a genuinely narrow (e.g. COLUMNS=60) terminal.
        let mut w = FeedWindow::new();
        w.stats.ok = 14; w.stats.rej = 2;
        w.record_rate(4.19);
        w.backend = Some("metal".into());
        let s = crate::theme::strip_ansi(&w.status_line_fx(60, false));
        assert!(
            s.chars().count() <= 60,
            "status line must not exceed the configured width: {} chars: {s:?}",
            s.chars().count()
        );
        assert!(s.ends_with('…'), "truncated status line must carry an ellipsis: {s:?}");
    }
    #[test]
    fn last_block_panel_and_session_din() {
        let mut w = FeedWindow::new();
        // solo: exact value (no ≈). 100 DIN = 100 × UNA_PER_DIN.
        w.record_block(1, "000000574714975b", "14:22:07", 100 * UNA_PER_DIN, false);
        let s = crate::theme::strip_ansi(&w.status_line_fx(200, false));
        assert!(s.contains("blocks 1") && s.contains("100.00 DIN") && !s.contains('≈'));
        let panel = crate::theme::strip_ansi(w.last_block.as_deref().unwrap());
        assert!(panel.contains("■ block #1") && panel.contains("14:22:07"));
        assert!(panel.contains("000000574714975b…"), "hash must have trailing ellipsis");
        // shared: estimated 45% of 100 DIN → total flips to ≈
        w.record_block(2, "0000003a861a070d", "15:01:44", 45 * UNA_PER_DIN, true);
        let s2 = crate::theme::strip_ansi(&w.status_line_fx(200, false));
        assert!(s2.contains("blocks 2") && s2.contains("≈145.00 DIN"));
        assert!(crate::theme::strip_ansi(w.last_block.as_deref().unwrap()).contains("0000003a861a070d…"), "hash must have trailing ellipsis");
    }
    #[test]
    fn repaint_region_lines_all_fit_within_width() {
        // Pins the whole-region invariant: every physical line the fixed
        // 10-row region emits must fit within `width`, or the region wraps
        // on a real terminal and desyncs the `\x1b[9F` cursor-up math. Uses
        // a REAL 64-hex-char block hash (what `hex::encode(found.hash)`
        // actually produces in both miners), not the short test hashes
        // used elsewhere in this file — the panel line was previously
        // never truncated and ran ~89 chars with a real hash, wrapping on
        // any terminal narrower than ~91 columns.
        let mut w = FeedWindow::new();
        w.push_row(feed_line(FeedKind::Share, 0x1234, &[0xabu8; 32], 80, false));
        w.record_rate(4.19);
        let real_hash = "0000005144829cc92581498e5f3d139cb0eddd556df74ee41cea3aea74e6e200";
        assert_eq!(real_hash.len(), 64, "sanity: this is a real block-hash length");
        w.record_block(1, real_hash, "05:38:41", 100 * UNA_PER_DIN, false);
        let width = 80;
        let out = w.repaint(width, false);
        for line in out.split('\n') {
            let stripped = crate::theme::strip_ansi(line);
            assert!(
                stripped.chars().count() <= width,
                "physical line exceeds width {width}: {} chars: {stripped:?}",
                stripped.chars().count()
            );
        }
    }
}
