//! Exact PPLNS checkpoints plus ordered, durable share appends.
//! Journal integrity protects unpaid contribution credits.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::accounting::{PplnsWindow, WindowEntry};

#[derive(serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    checkpoint_version: u32,
    entries: Vec<WindowEntry>,
}

pub struct WindowJournal {
    path: PathBuf,
    writer: BufWriter<File>,
    appends_since_compact: u64,
    failed: bool,
}

impl WindowJournal {
    pub const COMPACT_EVERY: u64 = 10_000;

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("mkdir {dir:?}"))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)
            .with_context(|| format!("open journal {path:?}"))?;
        Ok(Self { path: path.to_path_buf(), writer: BufWriter::new(file), appends_since_compact: 0, failed: false })
    }

    pub fn append(&mut self, entry: &WindowEntry) -> Result<()> {
        anyhow::ensure!(!self.failed, "journal requires recovery after an I/O failure");
        let result = (|| -> Result<()> {
            serde_json::to_writer(&mut self.writer, entry)?;
            self.writer.write_all(b"\n")?;
            self.writer.flush()?;
            self.writer.get_ref().sync_data()?;
            Ok(())
        })();
        if result.is_err() { self.failed = true; }
        result?;
        self.appends_since_compact += 1;
        Ok(())
    }

    /// Caller holds the journal mutex across this entire operation. This is
    /// the sole live credit path, keeping persistence, memory and snapshots
    /// in one order without holding the window mutex during disk writes.
    pub fn credit(&mut self, window: &std::sync::Mutex<PplnsWindow>, entry: &WindowEntry) -> Result<()> {
        self.append(entry)?;
        {
            let mut w = window.lock().expect("pplns window mutex");
            w.record(entry.payout_script.clone(), entry.weight, entry.unix_ts);
        }
        if self.should_compact() {
            let entries = window.lock().expect("pplns window mutex").entries().cloned().collect::<Vec<_>>();
            if let Err(e) = self.compact(&entries) {
                warn!(error = %e, "pplns journal compact failed");
            }
        }
        Ok(())
    }

    pub fn should_compact(&self) -> bool {
        self.appends_since_compact >= Self::COMPACT_EVERY
    }

    /// Load an exact checkpoint and replay subsequent appends. Legacy files
    /// have no boundary marker and require a verified live-window migration;
    /// guessing whether they contain full history or a compacted suffix can
    /// change credits. Never silently skip a damaged credited record.
    pub fn recover(path: &Path, target_secs: u64) -> Result<PplnsWindow> {
        Self::recover_records(path, target_secs, false)
    }

    fn recover_records(path: &Path, target_secs: u64, legacy_history: bool) -> Result<PplnsWindow> {
        if !path.exists() { return Ok(PplnsWindow::new(target_secs)); }
        let mut reader = BufReader::new(File::open(path)?);
        let mut window = PplnsWindow::new(target_secs);
        let mut line = Vec::new();
        let mut index = 0;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 { break; }
            // Only an unterminated final append can be a crash-torn write.
            // Startup rewrites the recovered prefix before accepting shares.
            if !line.ends_with(b"\n") {
                anyhow::ensure!(index > 0, "incomplete journal checkpoint/first record");
                warn!("ignoring incomplete final journal append");
                break;
            }
            let value: serde_json::Value = serde_json::from_slice(&line)
                .with_context(|| format!("corrupt journal record {index}"))?;
            if value.get("checkpoint_version").is_some() {
                anyhow::ensure!(index == 0, "checkpoint must be first journal record");
                let checkpoint: Checkpoint = serde_json::from_slice(&line)?;
                anyhow::ensure!(checkpoint.checkpoint_version == 1, "unknown journal checkpoint version");
                anyhow::ensure!(checkpoint.entries.len() <= PplnsWindow::CAP, "oversized checkpoint");
                window = PplnsWindow::restore(checkpoint.entries, target_secs);
            } else {
                anyhow::ensure!(index > 0 || legacy_history,
                    "legacy PPLNS journal requires verified live-window checkpoint migration; do not delete the journal");
                let entry: WindowEntry = serde_json::from_slice(&line)?;
                window.record(entry.payout_script, entry.weight, entry.unix_ts);
            }
            index += 1;
        }
        Ok(window)
    }

    pub fn compact(&mut self, entries: &[WindowEntry]) -> Result<()> {
        anyhow::ensure!(!self.failed, "journal requires recovery after an I/O failure");
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            serde_json::to_writer(&mut w, &Checkpoint {
                checkpoint_version: 1,
                entries: entries.to_vec(),
            })?;
            w.write_all(b"\n")?;
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        let file = OpenOptions::new().append(true).open(&tmp)?;
        std::fs::rename(&tmp, &self.path)?;
        // Switch descriptors immediately: never append to the unlinked old file.
        self.writer = BufWriter::new(file);
        #[cfg(unix)]
        File::open(self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?.sync_all()?;

        self.appends_since_compact = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(w: &PplnsWindow) -> Vec<WindowEntry> { w.entries().cloned().collect() }

    #[test]
    fn restart_and_compaction_match_uninterrupted_credits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut journal = WindowJournal::open(&path).unwrap();
        journal.compact(&[]).unwrap();
        let mut live = PplnsWindow::new(14400);
        let mut ts = 0;
        for i in 0..16000 {
            // Rate changes, quiet gaps, multiple contributors and weights.
            ts += if i % 4000 < 2000 { 30 } else { 1 };
            let e = WindowEntry { payout_script: vec![(i % 7) as u8], weight: (i % 13 + 1) as u128, unix_ts: ts };
            live.record(e.payout_script.clone(), e.weight, e.unix_ts);
            journal.append(&e).unwrap();
            if i % 997 == 0 {
                let recovered = WindowJournal::recover(&path, 14400).unwrap();
                assert_eq!(snapshot(&live), snapshot(&recovered), "restart at {i}");
                assert_eq!(live.weights(), recovered.weights());
                for script in 0..7 { assert_eq!(live.miner_bps(&[script]), recovered.miner_bps(&[script])); }
                journal.compact(&snapshot(&recovered)).unwrap();
                drop(journal);
                journal = WindowJournal::open(&path).unwrap();
                assert_eq!(snapshot(&live), snapshot(&WindowJournal::recover(&path, 14400).unwrap()));
            }
        }
        assert_eq!(snapshot(&live), snapshot(&WindowJournal::recover(&path, 14400).unwrap()));
    }

    #[test]
    fn full_history_single_eviction_regression() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut journal = WindowJournal::open(&path).unwrap();
        let mut live = PplnsWindow::new(14400);
        let mut ts = 0;
        for i in 0..5000 {
            ts += if i < 3000 { 30 } else { 1 };
            let e = WindowEntry { payout_script: vec![(i % 3) as u8], weight: 1, unix_ts: ts };
            live.record(e.payout_script.clone(), e.weight, e.unix_ts);
            serde_json::to_writer(&mut journal.writer, &e).unwrap();
            journal.writer.write_all(b"\n").unwrap();
        }
        journal.writer.flush().unwrap();
        assert!(WindowJournal::recover(&path, 14400).is_err());
        let recovered = WindowJournal::recover_records(&path, 14400, true).unwrap();
        assert_eq!(live.len(), recovered.len());
        assert_eq!(snapshot(&live), snapshot(&recovered));
    }

    #[test]
    fn concurrent_credit_and_compaction_recover_exactly_once() {
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let journal = Arc::new(Mutex::new(WindowJournal::open(&path).unwrap()));
        journal.lock().unwrap().compact(&[]).unwrap();
        let window = Arc::new(Mutex::new(PplnsWindow::new(14400)));
        let threads = (0..4).map(|miner| {
            let journal = journal.clone(); let window = window.clone();
            std::thread::spawn(move || {
                for i in 0..25 {
                    let mut j = journal.lock().unwrap();
                    // Force frequent compaction through the production path.
                    if i % 7 == 0 { j.appends_since_compact = WindowJournal::COMPACT_EVERY; }
                    j.credit(&window, &WindowEntry { payout_script: vec![miner], weight: 3, unix_ts: i }).unwrap();
                }
            })
        }).collect::<Vec<_>>();
        for thread in threads { thread.join().unwrap(); }
        let live = window.lock().unwrap();
        let recovered = WindowJournal::recover(&path, 14400).unwrap();
        assert_eq!(live.len(), 100);
        assert_eq!(snapshot(&live), snapshot(&recovered));
        assert_eq!(live.weights(), recovered.weights());
    }

    #[test]
    fn interrupted_compaction_keeps_old_journal_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut j = WindowJournal::open(&path).unwrap();
        let entries = vec![WindowEntry { payout_script: vec![2], weight: 9, unix_ts: 1 }];
        j.compact(&entries).unwrap();
        std::fs::write(path.with_extension("jsonl.tmp"), b"{incomplete replacement").unwrap();
        assert_eq!(snapshot(&WindowJournal::recover(&path, 14400).unwrap()), entries);
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_prevents_later_appends() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = WindowJournal::open(&dir.path().join("j.jsonl")).unwrap();
        // A read-only descriptor gives a deterministic write failure.
        j.writer = BufWriter::new(File::open(dir.path().join("j.jsonl")).unwrap());
        let entry = WindowEntry { payout_script: vec![1], weight: 1, unix_ts: 1 };
        assert!(j.append(&entry).is_err());
        assert!(j.append(&entry).unwrap_err().to_string().contains("requires recovery"));
        assert!(j.compact(&[]).is_err());
    }

    #[test]
    fn torn_tail_is_removed_before_new_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        let mut journal = WindowJournal::open(&path).unwrap();
        let entries = vec![WindowEntry { payout_script: vec![1], weight: 7, unix_ts: 10 }];
        journal.compact(&entries).unwrap();
        drop(journal);
        OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{torn").unwrap();
        let recovered = WindowJournal::recover(&path, 14400).unwrap();
        assert_eq!(snapshot(&recovered), entries);
        let mut journal = WindowJournal::open(&path).unwrap();
        journal.compact(&snapshot(&recovered)).unwrap();
        journal.append(&entries[0]).unwrap();
        assert_eq!(WindowJournal::recover(&path, 14400).unwrap().len(), 2);
    }

    #[test]
    fn corrupt_complete_record_fails_instead_of_discarding_credit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        for bytes in [b"{bad}\n".as_slice(), b"{\xff}\n", b"{torn", b"{\"checkpoint_version\":2,\"entries\":[]}\n"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(WindowJournal::recover(&path, 14400).is_err());
        }
    }
}
