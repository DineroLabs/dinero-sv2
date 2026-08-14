use std::time::Instant;

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
        if hs < 1e3 {
            format!("{} H/s", hs as u64)
        } else if hs < 1e6 {
            format!("{:.2} kH/s", hs / 1e3)
        } else if hs < 1e9 {
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
}
