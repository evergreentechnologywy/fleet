//! Cross-harness transcript search — one query over every agent's history.
//!
//! The fleet writes the same evidence into three JSONL stores that share no
//! format: omp (`~/.omp/agent/sessions`), Claude Code (`~/.claude/projects`) and
//! Codex (`~/.codex/sessions`). The engine's own `turn` table is a fourth. Until
//! this module, answering "what did we already decide about X" required
//! remembering WHICH harness said it and grepping that one directory by hand —
//! so in practice the answer was re-derived from scratch.
//!
//! The stores are read ON DEMAND. There is no index to go stale, no second copy
//! of ~150 MB of transcripts, and a session written a second ago is already
//! searchable. Deleting a session removes it from results with no rebuild.
//!
//! What is deliberately NOT searchable: harness plumbing. Codex replays its
//! whole context into `world_state` and `turn_context` records, and omp records
//! `custom` entries — the fleet's own `AGENTS.md` preamble appears in all of
//! them. Matching those would return every session for a query about any
//! standing rule, which is the same as returning nothing. Only records that
//! carry a conversation turn are read.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Which store a hit came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The engine's own `turn` table — sessions `fleet run` created.
    Fleet,
    /// omp agent sessions (`~/.omp/agent/sessions`).
    Omp,
    /// Claude Code project transcripts (`~/.claude/projects`).
    Claude,
    /// Codex rollout files (`~/.codex/sessions`).
    Codex,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Fleet => "fleet",
            Source::Omp => "omp",
            Source::Claude => "claude",
            Source::Codex => "codex",
        }
    }

    /// `all` is the default spelling on the command line, so it is accepted
    /// everywhere a source name is.
    pub fn parse(s: &str) -> Option<Source> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fleet" => Some(Source::Fleet),
            "omp" | "pi" => Some(Source::Omp),
            "claude" | "cc" => Some(Source::Claude),
            "codex" => Some(Source::Codex),
            _ => None,
        }
    }

    pub fn all() -> [Source; 4] {
        [Source::Fleet, Source::Omp, Source::Claude, Source::Codex]
    }
}

/// A directory to search and the format its files are in.
#[derive(Clone, Debug)]
pub struct Root {
    pub source: Source,
    pub dir: PathBuf,
}

/// The stores that exist under a home directory. A missing directory is not an
/// error: a node that has never run Codex simply has no Codex history.
pub fn default_roots(home: &Path) -> Vec<Root> {
    let candidates = [
        (Source::Omp, home.join(".omp/agent/sessions")),
        (Source::Claude, home.join(".claude/projects")),
        (Source::Codex, home.join(".codex/sessions")),
        (Source::Codex, home.join(".codex/archived_sessions")),
    ];
    candidates
        .into_iter()
        .filter(|(_, d)| d.is_dir())
        .map(|(source, dir)| Root { source, dir })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Maximum hits returned after sorting newest-first.
    pub limit: usize,
    /// Ignore sessions last modified longer ago than this.
    pub days: Option<i64>,
    /// Characters of context on each side of the match.
    pub context: usize,
    pub case_sensitive: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limit: 30,
            days: None,
            context: 100,
            case_sensitive: false,
        }
    }
}

/// One matching turn.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Hit {
    pub source: &'static str,
    /// The session identifier: a fleet session id, or the transcript file stem.
    pub session: String,
    pub path: String,
    pub ts: String,
    pub role: String,
    pub line: usize,
    pub snippet: String,
}

// ------------------------------------------------------------- text matching

/// Byte offset of `needle` in `hay`, ASCII-case-insensitively unless
/// `case_sensitive`. Returns only char boundaries, because the result is used
/// to slice `hay`.
fn find(hay: &str, needle: &str, case_sensitive: bool) -> Option<usize> {
    let n = needle.as_bytes();
    if n.is_empty() || hay.len() < n.len() {
        return None;
    }
    let h = hay.as_bytes();
    let mut i = 0;
    while i + n.len() <= h.len() {
        let matched = if case_sensitive {
            &h[i..i + n.len()] == n
        } else {
            h[i..i + n.len()].eq_ignore_ascii_case(n)
        };
        if matched && hay.is_char_boundary(i) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn contains(hay: &str, needle: &str, case_sensitive: bool) -> bool {
    find(hay, needle, case_sensitive).is_some()
}

/// Collapse runs of whitespace so a snippet reads as one line.
fn squeeze(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    out.trim().to_string()
}

/// A window of `text` around the first match, marked with `…` where it was cut.
fn snippet(text: &str, needle: &str, opts: &Options) -> String {
    let pos = match find(text, needle, opts.case_sensitive) {
        Some(p) => p,
        // The line matched before JSON decoding but the decoded text does not
        // (e.g. the hit was in a key name). Fall back to the head of the text.
        None => return squeeze(&truncate(text, opts.context * 2)),
    };
    let start = floor_boundary(text, pos.saturating_sub(opts.context));
    let end = ceil_boundary(text, (pos + needle.len() + opts.context).min(text.len()));
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(&squeeze(&text[start..end]));
    if end < text.len() {
        s.push('…');
    }
    s
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn truncate(s: &str, max: usize) -> String {
    let end = ceil_boundary(s, max.min(s.len()));
    s[..end].to_string()
}

// --------------------------------------------------------------- extraction

/// The text of a `content` field: a string, a list of parts, or one part.
fn content_text(c: &Value) -> Option<String> {
    let mut out = String::new();
    match c {
        Value::String(s) => out.push_str(s),
        Value::Array(items) => {
            for it in items {
                match it {
                    Value::String(s) => {
                        out.push_str(s);
                        out.push('\n');
                    }
                    Value::Object(o) => {
                        // Tool traffic is noise for a text search: the same
                        // bytes usually appear again in the surrounding prose,
                        // and tool output buries the turn that explains it.
                        let kind = o.get("type").and_then(|x| x.as_str()).unwrap_or("");
                        if matches!(
                            kind,
                            "tool_use" | "tool_result" | "function_call" | "function_call_output"
                        ) {
                            continue;
                        }
                        if let Some(t) = o.get("text").and_then(|x| x.as_str()) {
                            out.push_str(t);
                            out.push('\n');
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(o) => {
            if let Some(t) = o.get("text").and_then(|x| x.as_str()) {
                out.push_str(t);
            }
        }
        _ => {}
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `(timestamp, role, text)` for a transcript line, or `None` when the record is
/// not a conversation turn.
fn extract(source: Source, v: &Value) -> Option<(String, String, String)> {
    let ts = v
        .get("timestamp")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let (role, text) = match source {
        // omp: {"type":"message","message":{"role":…,"content":[…]}}
        Source::Omp => {
            if v.get("type").and_then(|x| x.as_str())? != "message" {
                return None;
            }
            let m = v.get("message")?;
            let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("");
            (role.to_string(), content_text(m.get("content")?)?)
        }
        // Claude Code: {"type":"user"|"assistant","message":{"role":…,…}}
        Source::Claude => {
            let m = v.get("message")?;
            if !m.is_object() {
                return None;
            }
            let role = m
                .get("role")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("type").and_then(|x| x.as_str()))
                .unwrap_or("");
            (role.to_string(), content_text(m.get("content")?)?)
        }
        // Codex: {"type":"response_item","payload":{"type":"message",…}}
        Source::Codex => {
            if v.get("type").and_then(|x| x.as_str())? != "response_item" {
                return None;
            }
            let p = v.get("payload")?;
            if p.get("type").and_then(|x| x.as_str())? != "message" {
                return None;
            }
            let role = p.get("role").and_then(|x| x.as_str()).unwrap_or("");
            (role.to_string(), content_text(p.get("content")?)?)
        }
        Source::Fleet => return None,
    };

    Some((ts, role, text))
}

// ------------------------------------------------------------------- walking

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        // Never follow symlinks: a link back up the tree would loop.
        let meta = match entry.file_type() {
            Ok(t) if t.is_symlink() => continue,
            Ok(_) => entry.metadata(),
            Err(_) => continue,
        };
        let meta = match meta {
            Ok(m) => m,
            Err(_) => continue,
        };
        let path = entry.path();
        if meta.is_dir() {
            jsonl_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn cutoff(opts: &Options) -> Option<SystemTime> {
    let days = opts.days?;
    let secs = days.max(0) as u64 * 86_400;
    SystemTime::now().checked_sub(std::time::Duration::from_secs(secs))
}

/// Search every transcript under `roots`, newest hit first.
pub fn search(needle: &str, roots: &[Root], opts: &Options) -> Result<Vec<Hit>> {
    if needle.trim().is_empty() {
        anyhow::bail!("nothing to search for");
    }
    let cut = cutoff(opts);
    // Read a bounded overshoot so the newest-first sort has something to sort.
    let cap = opts.limit.max(1).saturating_mul(8);
    let mut hits: Vec<Hit> = Vec::new();

    for root in roots {
        let mut files = Vec::new();
        jsonl_files(&root.dir, &mut files);
        files.sort();

        for path in files {
            if hits.len() >= cap {
                break;
            }
            if let Some(cut) = cut {
                match fs::metadata(&path).and_then(|m| m.modified()) {
                    Ok(m) if m < cut => continue,
                    _ => {}
                }
            }
            let body = match fs::read_to_string(&path) {
                Ok(b) => b,
                // A transcript being written right now can contain invalid
                // UTF-8 mid-line; reading it lossily still finds the match.
                Err(_) => match fs::read(&path) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(_) => continue,
                },
            };
            let session = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();

            for (idx, line) in body.lines().enumerate() {
                if !contains(line, needle, opts.case_sensitive) {
                    continue;
                }
                let parsed: Option<Value> = serde_json::from_str(line).ok();
                let (ts, role, text) = match parsed.as_ref().and_then(|v| extract(root.source, v)) {
                    Some(t) => t,
                    // A line that matches but is not a turn: plumbing, or a
                    // truncated write. Skip rather than report a false hit.
                    None => continue,
                };
                if !contains(&text, needle, opts.case_sensitive) {
                    continue;
                }
                hits.push(Hit {
                    source: root.source.as_str(),
                    session: session.clone(),
                    path: path.display().to_string(),
                    ts,
                    role,
                    line: idx + 1,
                    snippet: snippet(&text, needle, opts),
                });
                if hits.len() >= cap {
                    break;
                }
            }
        }
    }

    sort_and_trim(&mut hits, opts.limit);
    Ok(hits)
}

fn sort_and_trim(hits: &mut Vec<Hit>, limit: usize) {
    // Newest first; a hit with no timestamp sorts last rather than first.
    hits.sort_by(|a, b| {
        let (a_ts, b_ts) = (a.ts.as_str(), b.ts.as_str());
        match (a_ts.is_empty(), b_ts.is_empty()) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => b_ts.cmp(a_ts),
        }
    });
    hits.truncate(limit);
}

/// Search the engine's own `turn` table. Kept separate from the file walk
/// because the evidence is a database, not a transcript.
pub fn search_fleet_db(db: &Path, needle: &str, opts: &Options) -> Result<Vec<Hit>> {
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", db.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT session, ts, role, text FROM turn \
             WHERE instr(lower(text), lower(?1)) > 0 ORDER BY ts DESC LIMIT ?2",
        )
        .context("preparing the turn search")?;
    let rows = stmt
        .query_map(rusqlite::params![needle, opts.limit.max(1) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .context("querying turns")?;

    let mut hits = Vec::new();
    for row in rows.flatten() {
        let (session, ts, role, text) = row;
        hits.push(Hit {
            source: Source::Fleet.as_str(),
            session,
            path: db.display().to_string(),
            ts,
            role,
            line: 0,
            snippet: snippet(&text, needle, opts),
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            limit: 10,
            days: None,
            context: 20,
            case_sensitive: false,
        }
    }

    #[test]
    fn snippets_are_single_line_and_marked() {
        // The text must be long enough to ACTUALLY truncate, or the marker
        // assertions test nothing: with context=20 the original 35-char input
        // fitted whole, so there was no excerpt to mark and the test failed on
        // correct behaviour.
        let text = format!(
            "{} NEEDLE {}",
            "lorem ipsum dolor sit amet ".repeat(4),
            "consectetur adipiscing elit ".repeat(4)
        );
        let s = snippet(&text, "needle", &opts());
        assert!(s.contains("NEEDLE"), "{s}");
        assert!(!s.contains('\n'), "{s}");
        assert!(s.starts_with('…'), "no leading marker: {s}");
        assert!(s.ends_with('…'), "no trailing marker: {s}");
    }

    #[test]
    fn a_short_snippet_is_not_marked_as_excerpted() {
        // The other half of the same rule: do not put an ellipsis on a quote
        // that was never cut. A marker that appears unconditionally stops
        // meaning "there is more".
        let text = "alpha beta NEEDLE gamma";
        let s = snippet(text, "needle", &opts());
        assert!(s.contains("NEEDLE"), "{s}");
        assert!(!s.starts_with('…') && !s.ends_with('…'), "{s}");
    }

    #[test]
    fn case_insensitive_by_default() {
        assert!(contains("Fleet Model Policy", "model policy", false));
        assert!(!contains("Fleet Model Policy", "model policy", true));
    }

    #[test]
    fn plumbing_records_are_not_turns() {
        // The shape Codex uses to replay the whole context, including the
        // fleet's AGENTS.md preamble.
        let world: Value = serde_json::from_str(
            r#"{"type":"world_state","payload":{"state":{"agents_md":{"text":"model policy"}}}}"#,
        )
        .unwrap();
        assert!(extract(Source::Codex, &world).is_none());

        let omp_custom: Value = serde_json::from_str(
            r#"{"type":"custom","customType":"x","data":{"text":"model policy"}}"#,
        )
        .unwrap();
        assert!(extract(Source::Omp, &omp_custom).is_none());
    }

    #[test]
    fn each_harness_shape_yields_a_turn() {
        let omp: Value = serde_json::from_str(
            r#"{"type":"message","timestamp":"2026-10-27T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"hello policy"}]}}"#,
        )
        .unwrap();
        assert_eq!(extract(Source::Omp, &omp).unwrap().2.trim(), "hello policy");

        let claude: Value = serde_json::from_str(
            r#"{"type":"assistant","timestamp":"2026-10-27T10:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"hello policy"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            extract(Source::Claude, &claude).unwrap().2.trim(),
            "hello policy"
        );

        let codex: Value = serde_json::from_str(
            r#"{"type":"response_item","timestamp":"2026-10-27T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello policy"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            extract(Source::Codex, &codex).unwrap().2.trim(),
            "hello policy"
        );
    }
}
