use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_POOL: &str = "173.249.200.59:4444";
pub const DEFAULT_POOL_PUBKEY: &str =
    "3c879d90c9bb430493dfbf02cecbb93c3ae0d9d6c31d0757595e353fbe927417";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FileConfig {
    pub address: Option<String>,
    pub pool: Option<String>,
    pub server_pubkey: Option<String>,
    pub reward_mode: Option<String>,
    pub threads: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Effective {
    pub address: Option<String>,
    pub pool: String,
    pub server_pubkey: String,
    pub reward_mode: String,
    pub threads: usize,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("dinero-miner/config.json")
}

pub fn load(path: &std::path::Path) -> FileConfig {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => FileConfig::default(),
    }
}

pub fn save(path: &std::path::Path, c: &FileConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(c)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

pub fn resolve(flags: &FileConfig, file: &FileConfig, cores: usize) -> Effective {
    Effective {
        address: flags.address.as_ref().or(file.address.as_ref()).cloned(),
        pool: flags
            .pool
            .as_ref()
            .or(file.pool.as_ref())
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_POOL.to_string()),
        server_pubkey: flags
            .server_pubkey
            .as_ref()
            .or(file.server_pubkey.as_ref())
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_POOL_PUBKEY.to_string()),
        reward_mode: flags
            .reward_mode
            .as_ref()
            .or(file.reward_mode.as_ref())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "shared".to_string()),
        threads: flags
            .threads
            .or(file.threads)
            .unwrap_or_else(|| cores.saturating_sub(1).max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(
        address: Option<&str>,
        pool: Option<&str>,
        mode: Option<&str>,
        threads: Option<usize>,
    ) -> FileConfig {
        FileConfig {
            address: address.map(String::from),
            pool: pool.map(String::from),
            server_pubkey: None,
            reward_mode: mode.map(String::from),
            threads,
        }
    }

    #[test]
    fn defaults_when_nothing_set() {
        let e = resolve(&FileConfig::default(), &FileConfig::default(), 8);
        assert_eq!(e.pool, DEFAULT_POOL);
        assert_eq!(e.server_pubkey, DEFAULT_POOL_PUBKEY);
        assert_eq!(e.reward_mode, "shared");
        assert_eq!(e.threads, 7, "cores - 1");
        assert_eq!(e.address, None);
    }

    #[test]
    fn threads_floor_is_one() {
        assert_eq!(
            resolve(&FileConfig::default(), &FileConfig::default(), 1).threads,
            1
        );
    }

    #[test]
    fn file_overrides_default() {
        let e = resolve(
            &FileConfig::default(),
            &f(Some("din1x"), Some("h:1"), Some("solo"), Some(3)),
            8,
        );
        assert_eq!(
            (
                e.address.as_deref(),
                e.pool.as_str(),
                e.reward_mode.as_str(),
                e.threads
            ),
            (Some("din1x"), "h:1", "solo", 3)
        );
    }

    #[test]
    fn flag_overrides_file() {
        let e = resolve(
            &f(Some("din1flag"), None, Some("shared"), None),
            &f(Some("din1file"), Some("h:1"), Some("solo"), Some(3)),
            8,
        );
        assert_eq!(e.address.as_deref(), Some("din1flag"));
        assert_eq!(e.pool, "h:1"); // untouched by flags → file wins
        assert_eq!(e.reward_mode, "shared"); // flag wins
        assert_eq!(e.threads, 3); // file wins
    }

    #[test]
    fn load_missing_and_corrupt_yield_default() {
        let dir = std::env::temp_dir().join(format!("dm-ux-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        assert_eq!(load(&p), FileConfig::default());
        std::fs::write(&p, b"{nope").unwrap();
        assert_eq!(load(&p), FileConfig::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dm-ux-rt-{}", std::process::id()));
        let p = dir.join("config.json");
        let c = f(Some("din1saved"), None, Some("shared"), Some(5));
        save(&p, &c).unwrap(); // save must create parent dirs
        assert_eq!(load(&p), c);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
