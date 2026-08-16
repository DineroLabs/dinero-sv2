use std::time::Instant;
use std::collections::VecDeque;

/// The hash engine is the main visual attraction: keep a full 31-row rolling
/// window of real nonce/hash candidates at all times.
pub const FEED_HEIGHT: usize = 31;
pub const EVENT_HEIGHT: usize = 4;
/// Fixed dashboard below the permanent four-line logo. Titles and blank
/// placeholders are present from frame one so no live update can change the
/// physical height, wrap upward, or erase the DINERO header.
pub const REGION_LINES: usize = 1 + 4 + 1 + FEED_HEIGHT + 1 + 4 + 1 + EVENT_HEIGHT + 1;

/// Verified from dinero-v8/src/consensus/consensus.hpp:
/// `static constexpr int64_t COIN = 100'000'000;  // 1 DIN = 100,000,000 units (8 decimals)`
pub const UNA_PER_DIN: u64 = 100_000_000;

fn fit_plain(line: &str, width: usize) -> String {
    if line.chars().count() > width {
        let head: String = line.chars().take(width.saturating_sub(1)).collect();
        format!("{}…", head)
    } else {
        line.to_string()
    }
}

fn box_row(content: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let fitted = fit_plain(content, inner);
    format!("│{fitted:<inner$}│")
}

fn box_split(left: &str, right: &str, width: usize) -> String {
    let inner = width.saturating_sub(3);
    let left_width = inner / 2;
    let right_width = inner - left_width;
    let left = fit_plain(left, left_width);
    let right = fit_plain(right, right_width);
    format!("│{left:<left_width$}│{right:<right_width$}│")
}

fn box_rule(title: &str, width: usize) -> String {
    let prefix = format!("├─ {title} ");
    let fill = "─".repeat(width.saturating_sub(prefix.chars().count() + 1));
    format!("{prefix}{fill}┤")
}

fn clock_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() % 86_400)
        .unwrap_or(0);
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Compact, permanent DINERO banner + gold motto line. Three art rows are
/// half the height of the original six-row banner, leaving room for telemetry.
pub fn banner(colors: bool) -> String {
    let art_lines = [
        "█▀▄  █  █▄ █  █▀▀  █▀▄  █▀█",
        "█  █ █  █ ▀█  █▀   █▀▄  █ █",
        "█▄▀  █  █  █  █▄▄  █  █ █▄█",
    ];
    let width = crate::theme::term_width();

    let mut result = String::new();
    for line in &art_lines {
        let padding = " ".repeat(width.saturating_sub(line.chars().count()) / 2);
        result.push_str(&padding);
        result.push_str(&crate::theme::paint(crate::theme::BRIGHT_GREEN, line, colors));
        result.push('\n');
    }

    let motto = "· Real Money For Free People ·";
    result.push_str(&" ".repeat(width.saturating_sub(motto.chars().count()) / 2));
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

/// One feed row: `0x<nonce8> <full-64-char-hash> <marker>`. This is 77
/// columns, so a standard 80-column terminal shows every hash byte without
/// an ellipsis. Genuinely narrower terminals still truncate rather than wrap
/// and corrupt the fixed dashboard.
pub fn feed_line(kind: FeedKind, nonce: u32, hash: &[u8; 32], width: usize, colors: bool) -> String {
    let hash_hex = format!("{:02x?}", hash)
        .replace('[', "")
        .replace(']', "")
        .replace(", ", "")
        .to_lowercase();

    let marker = match kind {
        FeedKind::Candidate => "✗",
        FeedKind::Share => "✓",
        FeedKind::Rejected => "!",
        FeedKind::Stale => "↻",
    };

    let plain_row = format!("0x{:08x} {} {}", nonce, hash_hex, marker);

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
    pub pool: String,
    pub reward_mode: String,
    pub threads: usize,
    pub connection: String,
    pub channel: Option<u64>,
    pub target: Option<String>,
    pub reconnects: u64,
    pub pinned: bool,
    pub last_share: Option<Instant>,
    pub reward_address: String,
    rows: VecDeque<String>,
    rates: Vec<f64>,
    events: VecDeque<String>,
    painted: bool,
}

impl FeedWindow {
    pub fn new() -> Self {
        Self::with_session(String::new(), String::new(), 0, false, String::new())
    }

    pub fn with_session(pool: String, reward_mode: String, threads: usize, pinned: bool,
                        reward_address: String) -> Self {
        FeedWindow {
            stats: SessionStats::default(),
            backend: None,
            last_block: None,
            session_din_una: 0,
            din_estimated: false,
            pool,
            reward_mode,
            threads,
            connection: "STARTING".to_string(),
            channel: None,
            target: None,
            reconnects: 0,
            pinned,
            last_share: None,
            reward_address,
            rows: VecDeque::new(),
            rates: Vec::new(),
            events: VecDeque::new(),
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

    pub fn push_event(&mut self, event: String) {
        let event = event.trim_start_matches("»  ");
        self.events.push_back(format!(" {}  {}", clock_hms(), event));
        if self.events.len() > EVENT_HEIGHT {
            self.events.pop_front();
        }
    }

    pub fn set_lifecycle(&mut self, connection: Option<&str>, channel: Option<u64>,
                         target: Option<String>, reconnect: bool) {
        if let Some(value) = connection {
            self.connection = value.to_string();
        }
        if let Some(value) = channel {
            self.channel = Some(value);
        }
        if let Some(value) = target {
            self.target = Some(value);
        }
        if reconnect {
            self.reconnects += 1;
        }
    }

    fn console_telemetry(&self, width: usize, colors: bool) -> [String; 4] {
        let total = self.stats.ok + self.stats.rej;
        let success = if total == 0 { 100.0 } else { self.stats.ok as f64 * 100.0 / total as f64 };
        let ok_fill = ((success / 100.0) * 12.0).round() as usize;
        let ok_bar = format!("{}{}", "█".repeat(ok_fill), "▏".repeat(12 - ok_fill));
        let rejected_bar = if self.stats.rej == 0 { "·" } else { "▏" };
        let secure = if self.pinned { "NOISE NX PINNED" } else { "UNPINNED" };
        let last_share = self.last_share.map(|t| format!("{}s ago", t.elapsed().as_secs()))
            .unwrap_or_else(|| "waiting".to_string());
        let din = self.session_din_una as f64 / UNA_PER_DIN as f64;
        let prefix = if self.din_estimated || self.reward_mode == "shared" { "≈" } else { "" };
        let rows = [
            box_split(&format!(" ACCEPTED {:>6}  {ok_bar}", self.stats.ok),
                      &format!(" CONNECTION  ● {}", self.connection), width),
            box_split(&format!(" REJECTED {:>6}  {rejected_bar}", self.stats.rej),
                      &format!(" SECURITY    {secure}"), width),
            box_split(&format!(" SUCCESS   {:>6.1}%", success),
                      &format!(" RECONNECTS  {}", self.reconnects), width),
            box_split(&format!(" SESSION WORK  {prefix}{din:.2} DIN"),
                      &format!(" LAST SHARE   {last_share}"), width),
        ];
        rows.map(|row| crate::theme::paint(crate::theme::BRIGHT_GREEN, &row, colors))
    }

    pub fn record_block(&mut self, no: u64, hash: &str, local_time: &str, value_una: u64, estimated: bool) {
        self.stats.blocks += 1;
        self.session_din_una += value_una;
        if estimated {
            self.din_estimated = true;
        }
        let din = value_una as f64 / UNA_PER_DIN as f64;
        let prefix = if estimated { "≈" } else { "" };
        self.last_block = Some(format!(
            "  ■ block #{} · {} · {}… · {}{:.2} DIN",
            no, local_time, hash, prefix, din
        ));
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
        let width = width.max(60);
        let mut output = String::new();

        // Repaint only the fixed dashboard. The compact logo lives above this
        // region and is never part of the cursor-up/erase operation.
        if self.painted {
            output.push_str(&format!("\x1b[{}F\x1b[J", REGION_LINES - 1));
        }

        let top_title = " DINERO // SV2 MINING TERMINAL ";
        let top_fill = "─".repeat(width.saturating_sub(top_title.chars().count() + 3));
        output.push_str(&theme_line(&format!("╭─{top_title}{top_fill}╮"), colors));
        output.push_str("\x1b[K\n");
        let secure = if self.pinned { "● SECURE / NOISE NX" } else { "● UNPINNED" };
        let worker = if cfg!(target_arch = "x86_64") { "intel-mac" } else { "apple-silicon" };
        let uptime = self.stats.started.map(|s| Display::format_duration(s.elapsed().as_secs()))
            .unwrap_or_else(|| "0s".to_string());
        let channel = self.channel.map(|v| format!("#{v}")).unwrap_or_else(|| "--".to_string());
        for row in [
            box_split(&format!(" NODE  {}", self.pool), &format!(" LINK  {secure}"), width),
            box_split(&format!(" MODE  {} · PPLNS", self.reward_mode.to_uppercase()),
                      &format!(" WORKER  {worker} · {} threads", self.threads), width),
            box_split(&format!(" CHAN  {channel}"), &format!(" UPTIME  {uptime}"), width),
        ] {
            output.push_str(&theme_line(&row, colors));
            output.push_str("\x1b[K\n");
        }
        output.push_str(&theme_line(
            &box_row(&format!(" REWARD  {}", self.reward_address), width), colors));
        output.push_str("\x1b[K\n");
        let hash_title = format!("HASH ENGINE · {} · TARGET {}",
            Display::fmt_hashrate(self.stats.hashrate_hs),
            self.target.as_deref().map(|t| &t[..t.len().min(10)]).unwrap_or("pending"));
        output.push_str(&theme_line(&box_rule(&hash_title, width), colors));
        output.push_str("\x1b[K\n");

        // Feed rows: pad to FEED_HEIGHT with blanks
        for i in 0..FEED_HEIGHT {
            if i < self.rows.len() {
                let plain = crate::theme::strip_ansi(&self.rows[i]);
                output.push_str(&crate::theme::paint(crate::theme::DIM_GREEN,
                    &box_row(&plain, width), colors));
            } else {
                output.push_str(&box_row("", width));
            }
            output.push_str("\x1b[K\n");
        }

        output.push_str(&theme_line(&box_rule("SHARE TELEMETRY ────────────────┬─ SESSION HEALTH", width), colors));
        output.push_str("\x1b[K\n");
        for row in self.console_telemetry(width, colors) {
            output.push_str(&row);
            output.push_str("\x1b[K\n");
        }

        output.push_str(&theme_line(&box_rule("NETWORK FEED ───────────────────┴", width), colors));
        output.push_str("\x1b[K\n");
        for i in 0..EVENT_HEIGHT {
            if i < self.events.len() {
                output.push_str(&crate::theme::paint(crate::theme::DIM_GREEN,
                    &box_row(&self.events[i], width), colors));
            } else {
                output.push_str(&box_row("", width));
            }
            output.push_str("\x1b[K\n");
        }
        let footer = " ^C STOP │ [S] STATS │ [L] LOG │ [C] COMPACT ";
        let footer_fill = "─".repeat(width.saturating_sub(footer.chars().count() + 3));
        output.push_str(&theme_line(&format!("╰─{footer}{footer_fill}╯"), colors));

        self.painted = true;
        output
    }

    pub fn clear(&mut self) -> String {
        if !self.painted {
            return String::new();
        }

        let mut output = String::from("\r\x1b[K");
        for _ in 0..REGION_LINES - 1 {
            output.push_str("\x1b[1A\x1b[K");
        }

        self.painted = false;
        output
    }
}

fn theme_line(line: &str, colors: bool) -> String {
    crate::theme::paint(crate::theme::BRIGHT_GREEN, line, colors)
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
        assert!(plain.contains("█▀▄") && plain.contains("█▄▀"));
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
        assert_eq!(cand, format!("0x8f31a2c4 {} ✗", "0".repeat(64)));
        assert_eq!(cand.chars().count(), 77);
        let exact_80 = feed_line(FeedKind::Candidate, 0x8f31a2c4, &h, 80, false);
        assert_eq!(exact_80, cand, "80 columns must show the complete hash");
        assert!(!exact_80.contains('…'));
        let share = feed_line(FeedKind::Share, 1, &h, 100, false);
        assert!(share.ends_with('✓'));
        assert!(feed_line(FeedKind::Rejected, 1, &h, 100, false).ends_with('!'));
        assert!(feed_line(FeedKind::Stale, 1, &h, 100, false).ends_with('↻'));
        // A genuinely narrow terminal truncates rather than wrapping.
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
            "fixed dashboard has the same physical height from frame one");
        assert!(first.contains("row-a"));
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
        let repaint_prefix = format!("\x1b[{}F\x1b[J", REGION_LINES - 1);
        assert!(
            second.starts_with(&repaint_prefix),
            "later paints move across the dashboard only, then erase downward: {:?}",
            &second[..second.len().min(20)]
        );
        let clear = w.clear();
        assert_eq!(clear, format!("\r\x1b[K{}", "\x1b[1A\x1b[K".repeat(REGION_LINES - 1)));
        assert!(!w.clear().contains('\x1b'), "cleared window clears to nothing");
    }
    #[test]
    fn hash_engine_is_at_least_a_quarter_of_the_dashboard() {
        assert!(FEED_HEIGHT * 4 >= REGION_LINES);
    }
    #[test]
    fn network_feed_keeps_only_the_latest_events() {
        let mut w = FeedWindow::new();
        for n in 1..=5 {
            w.push_event(format!("event-{n}"));
        }
        let out = crate::theme::strip_ansi(&w.repaint(80, false));
        assert!(!out.contains("event-1"));
        assert!(out.contains("event-2") && out.contains("event-5"));
        assert!(out.contains("HASH ENGINE") && out.contains("NETWORK FEED"));
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
        // dashboard emits must fit within `width`, or the region wraps
        // on a real terminal and desyncs the fixed-region cursor math. Uses
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
