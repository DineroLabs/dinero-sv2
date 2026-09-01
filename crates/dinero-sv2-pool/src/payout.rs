//! Durable operator payout address.
//!
//! The address starts life in the systemd unit as `--payout-address`. Once an
//! operator changes it at runtime it must survive a restart, or the next
//! deploy silently reverts the fee to the unit's address and the operator
//! keeps mining for a destination they thought they had abandoned.
//!
//! So the file wins over the flag. The flag stays the installer's default and
//! the first-boot value; the file is "what the operator last chose".

use std::path::Path;

use anyhow::{Context, Result};
use tracing::warn;

use crate::ops::looks_like_payout_address;

/// Where a runtime-set address is persisted. Sits beside the ops token, in a
/// directory the installer already creates 0700.
pub const DEFAULT_PATH: &str = "/etc/dinero-sv2/payout-address";

/// Read a persisted override. `None` for missing, blank, or malformed — a
/// corrupt file must not be able to redirect the fee, and must not wedge
/// startup either, so we fall back to the flag and say so.
pub fn load_override(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let addr = raw.trim();
    if addr.is_empty() {
        return None;
    }
    if !looks_like_payout_address(addr) {
        warn!(
            path = %path.display(),
            "ignoring persisted payout address: not a plausible din1p address"
        );
        return None;
    }
    Some(addr.to_string())
}

/// Resolve the address to start with. Returns the address and whether it came
/// from the persisted file (for logging).
pub fn resolve_startup(flag: &str, path: &Path) -> (String, bool) {
    match load_override(path) {
        Some(a) => (a, true),
        None => (flag.to_string(), false),
    }
}

/// Persist atomically: write a temp file in the same directory, then rename.
/// A crash mid-write must never leave a half-written address behind, because
/// the next boot would read it and fall back — silently reverting the fee.
pub fn store(path: &Path, addr: &str) -> Result<()> {
    if !looks_like_payout_address(addr) {
        anyhow::bail!("refusing to persist implausible payout address");
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{addr}\n")).with_context(|| format!("writing {}", tmp.display()))?;
    restrict(&tmp)?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", p.display()))
}

#[cfg(not(unix))]
fn restrict(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy";
    // Charset-valid and distinct from GOOD. These tests never touch a chain —
    // the authoritative check is the trial getblocktemplate, by design.
    const OTHER: &str = "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxq";

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "payout-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_persisted_address_beats_the_flag() {
        let d = tmpdir();
        let p = d.join("payout-address");
        store(&p, OTHER).unwrap();
        assert_eq!(resolve_startup(GOOD, &p), (OTHER.to_string(), true));
    }

    #[test]
    fn the_flag_is_used_when_nothing_is_persisted() {
        let d = tmpdir();
        assert_eq!(
            resolve_startup(GOOD, &d.join("absent")),
            (GOOD.to_string(), false)
        );
    }

    // A truncated or hand-mangled file must not redirect the fee, and must not
    // stop the pool from booting either.
    #[test]
    fn a_corrupt_file_falls_back_to_the_flag() {
        let d = tmpdir();
        for junk in ["", "   \n", "not-an-address", "din1p", "bc1pqqqqqqqqqqqqqqqqqqqqqqqqqqq"] {
            let p = d.join("payout-address");
            std::fs::write(&p, junk).unwrap();
            assert_eq!(
                resolve_startup(GOOD, &p),
                (GOOD.to_string(), false),
                "junk: {junk:?}"
            );
        }
    }

    #[test]
    fn trailing_newline_and_whitespace_are_tolerated() {
        let d = tmpdir();
        let p = d.join("payout-address");
        std::fs::write(&p, format!("  {OTHER}  \n")).unwrap();
        assert_eq!(load_override(&p).as_deref(), Some(OTHER));
    }

    #[test]
    fn store_round_trips_and_overwrites() {
        let d = tmpdir();
        let p = d.join("payout-address");
        store(&p, GOOD).unwrap();
        assert_eq!(load_override(&p).as_deref(), Some(GOOD));
        store(&p, OTHER).unwrap();
        assert_eq!(load_override(&p).as_deref(), Some(OTHER));
    }

    #[test]
    fn store_refuses_junk_rather_than_persisting_it() {
        let d = tmpdir();
        let p = d.join("payout-address");
        assert!(store(&p, "not-an-address").is_err());
        assert!(!p.exists(), "nothing should have been written");
    }

    // A half-written file read at next boot would silently revert the fee.
    #[test]
    fn store_leaves_no_temp_file_behind() {
        let d = tmpdir();
        let p = d.join("payout-address");
        store(&p, GOOD).unwrap();
        assert!(!p.with_extension("tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn stored_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir();
        let p = d.join("payout-address");
        store(&p, GOOD).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }
}
