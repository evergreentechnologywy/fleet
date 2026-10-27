//! The ledger: append-only truth.
//!
//! Every routing decision, every harness call, every self-modification mark and
//! every lesson leaves a durable row here. The rule carried over from the
//! Octopus contracts: a failure is a row, never a silent drop. If something is
//! not in the ledger it did not happen, and if it is, it cannot be un-happened
//! by a later success.
//!
//! Format is JSON Lines so a run can be inspected with plain tools, and an
//! interrupted write costs at most one partial line.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub ts: String,
    /// `route`, `call`, `plan`, `mark`, `verify`, `lesson`, `harness`, `run`.
    pub kind: String,
    #[serde(default)]
    pub run: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub ok: bool,
    /// SHA-256 of the artifact a mark refers to, when there is one.
    #[serde(default)]
    pub digest: String,
}

pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> Self {
        Self::new(crate::paths::ledger_path())
    }

    /// Append one row. One `write_all` of a complete line, then `sync_all`, so
    /// a crash cannot leave a half-written row that a reader would misparse as
    /// a different event.
    pub fn append(&self, e: &Entry) -> Result<()> {
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p).ok();
        }
        let mut line = serde_json::to_string(e)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    pub fn log(&self, kind: &str, run: &str, session: &str, subject: &str, detail: &str, ok: bool) {
        let e = Entry {
            ts: crate::now(),
            kind: kind.to_string(),
            run: run.to_string(),
            session: session.to_string(),
            harness: String::new(),
            subject: subject.to_string(),
            detail: detail.to_string(),
            ok,
            digest: String::new(),
        };
        if let Err(err) = self.append(&e) {
            eprintln!("fleet: ledger append failed: {err}");
        }
    }

    /// Newest-first tail, optionally filtered by kind.
    pub fn tail(&self, kind: Option<&str>, limit: usize) -> Result<Vec<Entry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path)?;
        let mut out: Vec<Entry> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .filter(|e| kind.map(|k| e.kind == k).unwrap_or(true))
            .collect();
        out.reverse();
        out.truncate(limit);
        Ok(out)
    }
}

/// SHA-256 of a file's bytes — the identity a mark is recorded against.
pub fn digest_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(digest_bytes(&bytes))
}

pub fn digest_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}
