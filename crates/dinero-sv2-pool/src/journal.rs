//! Append-only JSONL persistence for the PPLNS window. Losing this
//! file never risks funds — only unpaid share *credit* (the window
//! rebuilds from new shares).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::accounting::{PplnsWindow, WindowEntry};

pub struct WindowJournal {
    path: PathBuf,
    writer: BufWriter<File>,
    appends_since_compact: u64,
}

impl WindowJournal {
    pub const COMPACT_EVERY: u64 = 10_000;

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("mkdir {dir:?}"))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)
            .with_context(|| format!("open journal {path:?}"))?;
        Ok(Self { path: path.to_path_buf(), writer: BufWriter::new(file), appends_since_compact: 0 })
    }

    pub fn append(&mut self, entry: &WindowEntry) -> Result<()> {
        serde_json::to_writer(&mut self.writer, entry)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.appends_since_compact += 1;
        Ok(())
    }

    pub fn should_compact(&self) -> bool {
        self.appends_since_compact >= Self::COMPACT_EVERY
    }

    pub fn load(path: &Path) -> Result<Vec<WindowEntry>> {
        if !path.exists() { return Ok(Vec::new()); }
        let reader = BufReader::new(File::open(path)?);
        let mut out = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<WindowEntry>(&line) {
                Ok(e) => out.push(e),
                Err(e) => warn!(line = i, error = %e, "skipping corrupt journal line"),
            }
        }
        Ok(out)
    }

    pub fn compact(&mut self, live: &PplnsWindow) -> Result<()> {
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for e in live.entries() {
                serde_json::to_writer(&mut w, e)?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.writer = BufWriter::new(file);
        self.appends_since_compact = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trip_and_corrupt_line_skip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut j = WindowJournal::open(&path).unwrap();
        j.append(&WindowEntry { payout_script: vec![0x51], weight: 5, unix_ts: 1 }).unwrap();
        j.append(&WindowEntry { payout_script: vec![0x52], weight: 6, unix_ts: 2 }).unwrap();
        drop(j);
        // Corrupt the middle of the file:
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{not json\n");
        std::fs::write(&path, raw).unwrap();
        let loaded = WindowJournal::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].weight, 6);
    }

    #[test]
    fn compact_rewrites_to_live_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut j = WindowJournal::open(&path).unwrap();
        for i in 0..10 {
            j.append(&WindowEntry { payout_script: vec![0x51], weight: 1, unix_ts: i }).unwrap();
        }
        let mut w = PplnsWindow::new(14_400);
        w.record(vec![0x51], 1, 9);
        j.compact(&w).unwrap();
        assert_eq!(WindowJournal::load(&path).unwrap().len(), 1);
    }
}
