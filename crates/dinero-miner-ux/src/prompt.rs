use std::io::{BufRead, Write};
use crate::address::validate_address;

#[derive(Debug, PartialEq)]
pub enum PromptOutcome {
    Address(String),
    Aborted,
}

/// Reads lines from `input`, writes prompts/errors to `out`. `saved`: offer
/// Enter-to-reuse. At most 3 invalid attempts, then Aborted. Pure I/O-trait
/// function — main() passes real stdin/stderr, tests pass cursors.
pub fn prompt_for_address(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    saved: Option<&str>,
) -> PromptOutcome {
    let mut strikes = 0;
    const MAX_STRIKES: usize = 3;

    loop {
        // Write prompt
        if let Some(saved_addr) = saved {
            write!(out, "mine to {}? [Enter = yes / paste a new address]: ", saved_addr).ok();
        } else {
            write!(out, "paste your Dinero address (din1p…): ").ok();
        }
        out.flush().ok();

        // Read line
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => return PromptOutcome::Aborted, // EOF
            Ok(_) => {}
            Err(_) => return PromptOutcome::Aborted,
        }

        let trimmed = line.trim();

        // Empty line with saved → reuse saved
        if trimmed.is_empty() {
            if let Some(saved_addr) = saved {
                writeln!(out, "  ✓ valid — saved for next time").ok();
                return PromptOutcome::Address(saved_addr.to_string());
            }
            // No saved and empty line: count as invalid attempt
            strikes += 1;
            if strikes >= MAX_STRIKES {
                return PromptOutcome::Aborted;
            }
            writeln!(out, "  ✗ Enter a Dinero address.").ok();
            continue;
        }

        // Validate
        match validate_address(trimmed) {
            Ok(normalized) => {
                writeln!(out, "  ✓ valid — saved for next time").ok();
                return PromptOutcome::Address(normalized);
            }
            Err(err) => {
                strikes += 1;
                writeln!(out, "  ✗ {}", err.message()).ok();
                if strikes >= MAX_STRIKES {
                    return PromptOutcome::Aborted;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GOOD: &str = "din1pafzgzwwfeqkfh7u4kkpe8qy97gey3zcvymx5eumxzx45m08q6tgqedz700";

    fn run(lines: &str, saved: Option<&str>) -> (PromptOutcome, String) {
        let mut inp = std::io::Cursor::new(lines.to_string());
        let mut out = Vec::new();
        let r = prompt_for_address(&mut inp, &mut out, saved);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn valid_first_try() {
        let (r, out) = run(&format!("{GOOD}\n"), None);
        assert!(matches!(r, PromptOutcome::Address(a) if a == GOOD));
        assert!(out.contains("paste your Dinero address"));
        assert!(out.contains("✓ valid"));
    }

    #[test]
    fn invalid_then_valid() {
        let (r, out) = run(&format!("dins1qq\n{GOOD}\n"), None);
        assert!(matches!(r, PromptOutcome::Address(_)));
        assert!(out.contains("✗"));
        assert!(out.contains("Shielded") || out.contains("dins1"));
    }

    #[test]
    fn three_strikes_aborts() {
        let (r, out) = run("bad\nbad\nbad\n", None);
        assert!(matches!(r, PromptOutcome::Aborted));
        assert_eq!(out.matches('✗').count(), 3);
    }

    #[test]
    fn enter_reuses_saved() {
        let (r, out) = run("\n", Some(GOOD));
        assert!(matches!(r, PromptOutcome::Address(a) if a == GOOD));
        assert!(out.contains("mine to"));
    }

    #[test]
    fn new_address_replaces_saved() {
        let other = "din1p977z3vkm5a2skmvlfvng4lxd9mnv95z43a38pastawrnc89gu7xsfcyczw";
        let (r, _) = run(&format!("{other}\n"), Some(GOOD));
        assert!(matches!(r, PromptOutcome::Address(a) if a == other));
    }

    #[test]
    fn eof_aborts() {
        let (r, _) = run("", None);
        assert!(matches!(r, PromptOutcome::Aborted));
    }
}
