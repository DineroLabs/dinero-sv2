use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::display::{self, celebration_frames, header_story_line, Display, FeedKind, FeedWindow};
use crate::theme;

/// A real candidate nonce/hash pulled from the miner's current sweep.
#[derive(Clone, Copy, Debug)]
pub struct CandidateSample {
    pub nonce: u32,
    pub hash: [u8; 32],
    pub header: [u8; 128],
}

/// Returns a REAL candidate from the miner's current sweep, or None
/// between jobs. Provided by each miner's main.rs.
pub type HashSampler = Arc<dyn Fn() -> Option<CandidateSample> + Send + Sync>;

pub struct FxConfig {
    pub width: usize,
    pub colors: bool,
    pub reward_mode: String,
    pub frame_delay_ms: u64,
    pub pool: String,
    pub threads: usize,
    pub pinned: bool,
    pub reward_address: String,
}

/// Block subsidy backing the "shared" reward-mode estimate.
///
/// Verified against dinero-v8 at implementation time:
/// `include/consensus/subsidy.h:66`:
/// `static constexpr uint64_t INITIAL_SUBSIDY = 100ULL * UNA_PER_DIN;   // 100 DIN per block`
/// pinned by the compile-time check at line 153:
/// `static_assert(INITIAL_SUBSIDY == 10000000000ULL, "Initial subsidy must be 100 DIN");`
pub const SHARED_BLOCK_SUBSIDY_UNA: u64 = 100 * display::UNA_PER_DIN;

struct Inner {
    window: FeedWindow,
    out: Box<dyn Write + Send>,
    cfg: FxConfig,
    last_sample: Option<CandidateSample>,
    last_window_bps: Option<u64>,
    last_solo_value_una: Option<u64>,
    tick_count: u64,
    alternate_screen: bool,
}

#[derive(Clone)]
pub struct FxScreen {
    inner: Arc<Mutex<Inner>>,
}

impl FxScreen {
    pub fn new(out: Box<dyn Write + Send>, cfg: FxConfig) -> Self {
        let mut window = FeedWindow::with_session(
            cfg.pool.clone(), cfg.reward_mode.clone(), cfg.threads, cfg.pinned,
            cfg.reward_address.clone());
        window.stats.started = Some(Instant::now());
        FxScreen {
            inner: Arc::new(Mutex::new(Inner {
                window,
                out,
                cfg,
                last_sample: None,
                last_window_bps: None,
                last_solo_value_una: None,
                tick_count: 0,
                alternate_screen: false,
            })),
        }
    }

    fn write_flush(inner: &mut Inner, s: &str) {
        let _ = inner.out.write_all(s.as_bytes());
        let _ = inner.out.flush();
    }

    /// Enter a clean alternate screen, place the permanent logo at row one,
    /// then leave a blank line before the fixed dashboard. The user's shell
    /// history remains intact in the primary screen and returns on exit.
    pub fn print_banner(&self) {
        let mut inner = self.inner.lock().unwrap();
        let colors = inner.cfg.colors;
        let mut out = String::from("\x1b[?1049h\x1b[2J\x1b[H");
        inner.alternate_screen = true;
        out.push_str(&display::banner(colors));
        out.push('\n'); // blank line under the banner
        Self::write_flush(&mut inner, &out);
    }

    /// Keep lifecycle activity inside the dashboard's network feed. Nothing
    /// is printed above or below the fixed region, so the logo remains pinned.
    pub fn lifecycle(&self, line: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.window.push_event(line.to_string());
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    pub fn lifecycle_state(&self, line: &str, connection: Option<&str>,
                           channel: Option<u64>, target: Option<String>, reconnect: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.window.set_lifecycle(connection, channel, target, reconnect);
        inner.window.push_event(line.to_string());
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    pub fn set_backend(&self, backend: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.window.backend = Some(backend.to_string());
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    /// record_rate → repaint.
    pub fn on_hashrate(&self, mhs: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.window.record_rate(mhs);
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    /// stats.ok += n; Share feed row uses last sample (or a plain
    /// text row when no sample has ever been seen).
    pub fn on_share_ok(&self, n: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.window.stats.ok += n;
        inner.window.last_share = Some(Instant::now());
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let row = match inner.last_sample {
            Some(s) => header_story_line(FeedKind::Share, s.nonce, &s.hash, &s.header, width, colors),
            None => theme::paint(theme::BRIGHT_GREEN, "  ▓ SHARE ✓ pool accepted", colors),
        };
        inner.window.push_row(row);
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    /// stats.rej += 1; Rejected row.
    pub fn on_share_rejected(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.window.stats.rej += 1;
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let row = match inner.last_sample {
            Some(s) => header_story_line(FeedKind::Rejected, s.nonce, &s.hash, &s.header, width, colors),
            None => theme::paint(theme::RED, "  ✗ rejected", colors),
        };
        inner.window.push_row(row);
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    fn push_candidate(inner: &mut Inner, s: CandidateSample) -> String {
        inner.last_sample = Some(s);
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let row = header_story_line(FeedKind::Candidate, s.nonce, &s.hash, &s.header, width, colors);
        inner.window.push_row(row);
        inner.window.repaint(width, colors)
    }

    /// Candidate row → repaint.
    pub fn on_candidate(&self, s: CandidateSample) {
        let mut inner = self.inner.lock().unwrap();
        let out = Self::push_candidate(&mut inner, s);
        Self::write_flush(&mut inner, &out);
    }

    /// Latest PPLNS window share, from `window_status` events (basis points).
    pub fn on_window(&self, bps: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_window_bps = Some(bps);
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    /// Latest solo-template coinbase value, from solo `new_job` events.
    pub fn on_solo_job_value(&self, una: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_solo_value_una = Some(una);
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let out = inner.window.repaint(width, colors);
        Self::write_flush(&mut inner, &out);
    }

    /// Celebration (owner calls: bottom of screen, NO permanent banner).
    /// Freeze feed; write "\n" + frame + "\x1b[K", then overwrite that
    /// same line per frame with "\r" + frame + "\x1b[K" (sleep
    /// frame_delay_ms between frames); erase region + frame line
    /// (clear variant covering REGION_LINES + 1 lines); then
    /// `record_block` on the window and repaint — the last-block panel
    /// and the status DIN total are the only lasting trace.
    pub fn on_block(&self, hash_hex: &str, local_time: &str) {
        let mut inner = self.inner.lock().unwrap();
        let width = inner.cfg.width;
        let colors = inner.cfg.colors;
        let block_no = inner.window.stats.blocks + 1;
        let frames = celebration_frames(width, block_no, colors);
        let frame_delay = inner.cfg.frame_delay_ms;

        if let Some(first) = frames.first() {
            let mut out = String::from("\n");
            out.push_str(first);
            out.push_str("\x1b[K");
            Self::write_flush(&mut inner, &out);
        }
        for frame in frames.iter().skip(1) {
            if frame_delay > 0 {
                thread::sleep(Duration::from_millis(frame_delay));
            }
            let mut out = String::from("\r");
            out.push_str(frame);
            out.push_str("\x1b[K");
            Self::write_flush(&mut inner, &out);
        }

        // Erase region + frame line: clear the frame line itself, step
        // up onto the (former) status line, then erase the fixed
        // REGION_LINES region via the same routine `clear()` uses
        // elsewhere — REGION_LINES + 1 lines total.
        let mut erase = String::from("\r\x1b[K\x1b[1A");
        erase.push_str(&inner.window.clear());

        let reward_mode = inner.cfg.reward_mode.clone();
        let value_una = if reward_mode == "solo" {
            inner.last_solo_value_una.unwrap_or(0)
        } else {
            // PPLNS window share can't exceed 100%; clamp so a malformed
            // pool `window_status` bps value can't overflow the multiply.
            let bps = inner.last_window_bps.unwrap_or(10_000).min(10_000);
            SHARED_BLOCK_SUBSIDY_UNA * bps / 10_000
        };
        let estimated = reward_mode != "solo";
        inner
            .window
            .record_block(block_no, hash_hex, local_time, value_una, estimated);
        if let Some(block) = inner.window.last_block.clone() {
            inner.window.push_event(block);
        }

        let mut out = erase;
        out.push_str(&inner.window.repaint(width, colors));
        Self::write_flush(&mut inner, &out);
    }

    /// clear region, v1 session_summary + FX's session DIN total (spec:
    /// "the exit summary includes the same total" as the status line),
    /// painted BOLD when colors are on. v1 `Display::session_summary`
    /// itself is untouched — the DIN segment is appended here so the
    /// --plain/v1 output stays byte-stable.
    pub fn print_summary(&self) {
        let mut inner = self.inner.lock().unwrap();
        let mut out = inner.window.clear();
        let elapsed = inner
            .window
            .stats
            .started
            .map(|s| s.elapsed().as_secs())
            .unwrap_or(0);
        let colors = inner.cfg.colors;
        let mut line = Display::session_summary(&inner.window.stats, elapsed);
        if inner.window.stats.blocks > 0 {
            let din_total = inner.window.session_din_una as f64 / display::UNA_PER_DIN as f64;
            let din_prefix = if inner.window.din_estimated { "≈" } else { "" };
            line.push_str(&format!(" | {}{:.2} DIN", din_prefix, din_total));
        }
        if inner.alternate_screen {
            out.push_str("\x1b[?1049l");
            inner.alternate_screen = false;
        }
        out.push_str(&theme::paint(theme::BOLD, &line, colors));
        out.push('\n');
        Self::write_flush(&mut inner, &out);
    }

    /// 10 Hz loop calling `tick` until `stop` is true. Detached std thread.
    pub fn spawn_ticker(&self, sampler: HashSampler, stop: Arc<AtomicBool>) {
        let screen = self.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                screen.tick(&sampler);
                thread::sleep(Duration::from_millis(100));
            }
        });
    }

    /// One sample → on_candidate; test seam.
    pub fn tick(&self, sampler: &HashSampler) {
        let mut inner = self.inner.lock().unwrap();
        inner.tick_count = inner.tick_count.wrapping_add(1);
        match sampler() {
            Some(sample) => {
                let out = Self::push_candidate(&mut inner, sample);
                Self::write_flush(&mut inner, &out);
            }
            None => {
                // Still repaint every 5th tick so uptime advances even
                // when there's nothing new to show.
                if inner.tick_count % 5 == 0 {
                    let width = inner.cfg.width;
                    let colors = inner.cfg.colors;
                    let out = inner.window.repaint(width, colors);
                    Self::write_flush(&mut inner, &out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn screen_with_buffer() -> (FxScreen, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer: Box<dyn io::Write + Send> = Box::new(SharedBuf(buf.clone()));
        let cfg = FxConfig {
            width: 80,
            colors: false,
            reward_mode: "shared".to_string(),
            frame_delay_ms: 0,
            pool: "pool.dinerolabs.org:4444".to_string(),
            threads: 2,
            pinned: true,
            reward_address: "din1ptestrewardaddress".to_string(),
        };
        let fx = FxScreen::new(writer, cfg);
        // Seed a realistic rate so the status line's unit formatting is
        // exercised the way a live miner would drive it. Clear the byte
        // buffer afterward so assertions target what the test itself
        // triggers, not this setup repaint — `stats.hashrate_hs` persists
        // in `FeedWindow` regardless, so later repaints still carry it.
        fx.on_hashrate(4.2);
        buf.lock().unwrap().clear();
        (fx, buf)
    }

    #[test]
    fn tick_feeds_real_sample_and_repaints() {
        let (fx, buf) = screen_with_buffer();
        let sampler: HashSampler = std::sync::Arc::new(|| {
            Some(CandidateSample {
                nonce: 0xabcd0001,
                hash: [7u8; 32],
                header: [0u8; 128],
            })
        });
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
        assert!(plain.contains("ACCEPTED      3") && plain.contains("REJECTED      1"));
        assert!(plain.contains("SHARE ✓") && plain.contains("✗ rejected"));
    }

    #[test]
    fn print_summary_carries_din_total_colored_bold() {
        let (fx, buf) = screen_with_buffer();
        fx.on_window(4500); // 45% PPLNS window
        fx.on_block("000000574714975b", "14:22:07");
        buf.lock().unwrap().clear();
        fx.print_summary();
        let raw = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        // colors:false in screen_with_buffer() — no SGR color codes, even
        // though clear() still emits cursor-movement CSI sequences.
        assert!(
            !raw.contains(theme::BOLD) && !raw.contains(theme::RESET),
            "colors:false — no BOLD/RESET SGR in summary"
        );
        let plain = crate::theme::strip_ansi(&raw);
        assert!(
            plain.contains("≈45.00 DIN"),
            "exit summary carries the session DIN total: {plain:?}"
        );
    }

    #[test]
    fn print_summary_omits_din_when_no_blocks() {
        let (fx, buf) = screen_with_buffer();
        fx.print_summary();
        let plain = crate::theme::strip_ansi(&String::from_utf8(buf.lock().unwrap().clone()).unwrap());
        assert!(!plain.contains("DIN"), "no blocks found -> no DIN total in summary: {plain:?}");
    }

    #[test]
    fn banner_uses_alternate_screen_and_summary_restores_shell() {
        let (fx, buf) = screen_with_buffer();
        fx.print_banner();
        let entered = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(entered.starts_with("\x1b[?1049h\x1b[2J\x1b[H"));
        assert!(crate::theme::strip_ansi(&entered).contains("Real Money For Free People"));

        buf.lock().unwrap().clear();
        fx.print_summary();
        let restored = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(restored.contains("\x1b[?1049l"));
        assert!(restored.contains("Session:"));
    }

    #[test]
    fn ticker_thread_stops_on_flag() {
        let (fx, _buf) = screen_with_buffer();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let n2 = n.clone();
        let sampler: HashSampler = std::sync::Arc::new(move || {
            n2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        });
        fx.spawn_ticker(sampler, stop.clone());
        std::thread::sleep(std::time::Duration::from_millis(350));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let calls = n.load(std::sync::atomic::Ordering::Relaxed);
        assert!(calls >= 2, "ticker ran ({calls} calls)");
        let at_stop = calls;
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(n.load(std::sync::atomic::Ordering::Relaxed) <= at_stop + 1, "stopped");
    }
}
