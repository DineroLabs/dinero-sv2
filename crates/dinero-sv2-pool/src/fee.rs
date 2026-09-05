//! Durable runtime operator-fee policy.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::warn;

pub const DEFAULT_PATH: &str = "/etc/dinero-sv2/shared-fee-bps";
pub const MAX_BPS: u32 = 10_000;

pub fn load_override(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    match raw.trim().parse::<u32>() {
        Ok(value) if value <= MAX_BPS => Some(value),
        _ => {
            warn!(path = %path.display(), "ignoring invalid persisted operator fee");
            None
        }
    }
}

pub fn resolve_startup(flag: u32, path: &Path) -> (u32, bool) {
    match load_override(path) {
        Some(value) => (value, true),
        None => (flag.min(MAX_BPS), false),
    }
}

pub fn store(path: &Path, value: u32) -> Result<()> {
    if value > MAX_BPS {
        anyhow::bail!("operator fee must be between 0 and 10000 basis points");
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{value}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    restrict(&tmp)?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fee-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn persisted_fee_beats_flag_and_survives_restart() {
        let path = tmpdir().join("fee");
        store(&path, 500).unwrap();
        assert_eq!(resolve_startup(200, &path), (500, true));
    }

    #[test]
    fn corrupt_or_out_of_range_override_falls_back() {
        let path = tmpdir().join("fee");
        for invalid in ["", "nope", "10001", "-1"] {
            std::fs::write(&path, invalid).unwrap();
            assert_eq!(resolve_startup(250, &path), (250, false));
        }
    }

    #[test]
    fn store_accepts_boundaries_and_rejects_overflow() {
        let path = tmpdir().join("fee");
        store(&path, 0).unwrap();
        assert_eq!(load_override(&path), Some(0));
        store(&path, MAX_BPS).unwrap();
        assert_eq!(load_override(&path), Some(MAX_BPS));
        assert!(store(&path, MAX_BPS + 1).is_err());
        assert_eq!(load_override(&path), Some(MAX_BPS));
    }
}
