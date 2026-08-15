pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM_GREEN: &str = "\x1b[2;32m";     // feed candidates
pub const BRIGHT_GREEN: &str = "\x1b[1;92m";  // shares / accents
pub const GOLD: &str = "\x1b[1;33m";          // blocks / motto
pub const RED: &str = "\x1b[1;31m";           // rejected
pub const YELLOW: &str = "\x1b[33m";          // stale notices
pub const FAINT: &str = "\x1b[2m";            // rules / separators

/// Wraps `s` in `code` + RESET when `colors`, else returns `s` verbatim.
pub fn paint(code: &str, s: &str, colors: bool) -> String {
    if colors {
        format!("{}{}{}", code, s, RESET)
    } else {
        s.to_string()
    }
}

/// Removes CSI escape sequences (a small state machine, no regex).
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Check if this is the start of a CSI sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Skip characters until we find a character in the range '@'..='~'
                while let Some(c) = chars.next() {
                    if c >= '@' && c <= '~' {
                        break;
                    }
                }
            } else {
                // Not a CSI sequence, emit the escape char
                result.push(ch);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// NO_COLOR convention: colors are on iff the env var is unset.
pub fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// Helper: TERM detection without environment access.
/// Returns false if term is None, empty, or "dumb"; otherwise true.
fn term_ok(term: Option<&str>) -> bool {
    match term {
        None => false,
        Some("") => false,
        Some("dumb") => false,
        Some(_) => true,
    }
}

/// Spec detection rule: TERM unset, empty, or "dumb" → no FX.
pub fn term_supports_fx() -> bool {
    term_ok(std::env::var("TERM").ok().as_deref())
}

/// Helper: parse COLUMNS env var and clamp to >= 60; fallback to 100.
fn width_from(columns: Option<String>) -> usize {
    match columns {
        None => 100,
        Some(s) => {
            match s.trim().parse::<usize>() {
                Ok(w) => w.max(60),
                Err(_) => 100,
            }
        }
    }
}

/// COLUMNS env → `tput cols` (one spawn) → 100. Clamped to >= 60.
pub fn term_width() -> usize {
    // Try COLUMNS env var first
    if let Ok(cols_str) = std::env::var("COLUMNS") {
        return width_from(Some(cols_str));
    }

    // Try tput cols
    if let Ok(output) = std::process::Command::new("tput")
        .arg("cols")
        .output()
    {
        if output.status.success() {
            if let Ok(s) = String::from_utf8(output.stdout) {
                if let Ok(w) = s.trim().parse::<usize>() {
                    return w.max(60);
                }
            }
        }
    }

    // Fallback
    100
}

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
