//! Append-only JSONL persistence for the PPLNS window. Losing this
//! file never risks funds — only unpaid share *credit* (the window
//! rebuilds from new shares).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::accounting::WindowEntry;

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
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    // A read/UTF-8 decode error usually means the file is
                    // truncated or corrupted from this point on (e.g. a
                    // crash mid-write). Stop reading further and return
                    // what was successfully parsed so far, rather than
                    // failing the whole load and aborting pool startup.
                    warn!(
                        line = i,
                        error = %e,
                        "journal unreadable from this line onward — stopping load, keeping entries read so far"
                    );
                    break;
                }
            };
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<WindowEntry>(&line) {
                Ok(e) => out.push(e),
                Err(e) => warn!(line = i, error = %e, "skipping corrupt journal line"),
            }
        }
        Ok(out)
    }

    pub fn compact(&mut self, entries: &[WindowEntry]) -> Result<()> {
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for e in entries {
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
    use crate::accounting::PplnsWindow;

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
    fn journal_load_stops_at_non_utf8_corruption_but_keeps_prior_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut j = WindowJournal::open(&path).unwrap();
        j.append(&WindowEntry { payout_script: vec![0x51], weight: 5, unix_ts: 1 }).unwrap();
        j.append(&WindowEntry { payout_script: vec![0x52], weight: 6, unix_ts: 2 }).unwrap();
        drop(j);
        // Inject raw non-UTF-8 bytes (simulating truncation/corruption
        // mid-write) between valid lines, followed by another valid line
        // that must NOT be recovered since we stop at the first bad line.
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"\xff\xfe{bad\n");
        raw.extend_from_slice(
            serde_json::to_string(&WindowEntry { payout_script: vec![0x53], weight: 7, unix_ts: 3 })
                .unwrap()
                .as_bytes(),
        );
        raw.push(b'\n');
        std::fs::write(&path, raw).unwrap();

        // load() must not error — it should return the entries read
        // before the corruption, and log loudly rather than aborting.
        let loaded = WindowJournal::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].weight, 5);
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
        let entries: Vec<WindowEntry> = w.entries().cloned().collect();
        j.compact(&entries).unwrap();
        assert_eq!(WindowJournal::load(&path).unwrap().len(), 1);
    }

    #[test]
    fn compact_writes_exactly_the_snapshot_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut j = WindowJournal::open(&path).unwrap();
        for i in 0..5 {
            j.append(&WindowEntry { payout_script: vec![0x51], weight: 1, unix_ts: i }).unwrap();
        }
        // Snapshot only two entries — compact must write exactly these,
        // regardless of what was previously appended to the journal.
        let entries = vec![
            WindowEntry { payout_script: vec![0x52], weight: 7, unix_ts: 100 },
            WindowEntry { payout_script: vec![0x53], weight: 8, unix_ts: 101 },
        ];
        j.compact(&entries).unwrap();
        let loaded = WindowJournal::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].payout_script, vec![0x52]);
        assert_eq!(loaded[0].weight, 7);
        assert_eq!(loaded[1].payout_script, vec![0x53]);
        assert_eq!(loaded[1].weight, 8);
    }
}
