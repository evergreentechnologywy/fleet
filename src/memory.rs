//! Shared memory and session store — one SQLite database in WAL mode.
//!
//! Guarantees the fleet requires:
//!
//! * **Instant.** WAL + `synchronous=FULL`, one transaction per append. A write
//!   returns only after it is durable, so a harness that reads the next moment
//!   sees it. No queueing layer, no eventual flush.
//! * **Last version wins.** `memory` is keyed `(scope, key)`; a newer write
//!   supersedes the older row in place, and every superseded value is kept in
//!   `memory_version`. Readers get the newest; nothing is lost.
//! * **Never lose a conversation.** Every turn is appended to `turn` keyed by
//!   `(session, seq)`, with the harness, model that served it, and full text.
//!   A resumed run replays exactly what happened, in order, including any turn
//!   that was in flight when the process died.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct Memory {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub session: String,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub harness: String,
    pub model: String,
    pub text: String,
    pub run_id: String,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created: String,
    pub updated: String,
    pub status: String,
    pub turns: i64,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA busy_timeout=10000;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS memory (
  scope      TEXT NOT NULL,
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  version    INTEGER NOT NULL DEFAULT 1,
  author     TEXT NOT NULL DEFAULT '',
  updated    TEXT NOT NULL,
  PRIMARY KEY (scope, key)
);

CREATE TABLE IF NOT EXISTS memory_version (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  scope      TEXT NOT NULL,
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  version    INTEGER NOT NULL,
  author     TEXT NOT NULL,
  replaced   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memver ON memory_version(scope, key, id DESC);

CREATE TABLE IF NOT EXISTS session (
  id         TEXT PRIMARY KEY,
  title      TEXT NOT NULL DEFAULT '',
  created    TEXT NOT NULL,
  updated    TEXT NOT NULL,
  status     TEXT NOT NULL DEFAULT 'open'
);

CREATE TABLE IF NOT EXISTS turn (
  session    TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  ts         TEXT NOT NULL,
  role       TEXT NOT NULL,
  harness    TEXT NOT NULL DEFAULT '',
  model      TEXT NOT NULL DEFAULT '',
  text       TEXT NOT NULL,
  run_id     TEXT NOT NULL DEFAULT '',
  ok         INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (session, seq)
);
CREATE INDEX IF NOT EXISTS idx_turn_run ON turn(run_id);

CREATE TABLE IF NOT EXISTS run (
  id         TEXT PRIMARY KEY,
  session    TEXT NOT NULL,
  task       TEXT NOT NULL,
  plan       TEXT NOT NULL,
  status     TEXT NOT NULL,
  started    TEXT NOT NULL,
  finished   TEXT,
  detail     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_run_session ON run(session, started DESC);
"#;

impl Memory {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Self { conn })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&crate::paths::db_path())
    }

    /// Durably write `value` at `(scope, key)`, superseding any previous value.
    /// Returns the new version number.
    pub fn put(&self, scope: &str, key: &str, value: &str, author: &str) -> Result<i64> {
        let now = crate::now();
        let tx = self.conn.unchecked_transaction()?;
        let prev: Option<(String, i64)> = tx
            .query_row(
                "SELECT value, version FROM memory WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let version = match prev {
            Some((old, v)) => {
                tx.execute(
                    "INSERT INTO memory_version(scope,key,value,version,author,replaced)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![scope, key, old, v, author, now],
                )?;
                v + 1
            }
            None => 1,
        };
        tx.execute(
            "INSERT INTO memory(scope,key,value,version,author,updated)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(scope,key) DO UPDATE SET
               value=excluded.value, version=excluded.version,
               author=excluded.author, updated=excluded.updated",
            params![scope, key, value, version, author, now],
        )?;
        tx.commit()?;
        Ok(version)
    }

    pub fn get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM memory WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Newest-first listing of a scope, optionally filtered by a key prefix.
    pub fn list(&self, scope: &str, prefix: &str, limit: i64) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT key, value FROM memory
             WHERE scope=?1 AND (?2='' OR key LIKE ?2 || '%')
             ORDER BY updated DESC, key ASC LIMIT ?3",
        )?;
        let rows = st
            .query_map(params![scope, prefix, limit], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Full history of one key, newest first. This is how a lost value is
    /// recovered: the value is never deleted, only superseded.
    pub fn history(
        &self,
        scope: &str,
        key: &str,
        limit: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT version, value, replaced FROM memory_version
             WHERE scope=?1 AND key=?2 ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st
            .query_map(params![scope, key, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Substring search across the live memory, newest first.
    pub fn search(&self, needle: &str, limit: i64) -> Result<Vec<(String, String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT scope, key, value FROM memory
             WHERE key LIKE '%'||?1||'%' OR value LIKE '%'||?1||'%' OR scope LIKE '%'||?1||'%'
             ORDER BY updated DESC LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![needle, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- sessions -------------------------------------------------------

    /// Open a session, creating it if new. Returns the id.
    pub fn session_open(&self, id: &str, title: &str) -> Result<String> {
        let now = crate::now();
        self.conn.execute(
            "INSERT INTO session(id,title,created,updated) VALUES(?1,?2,?3,?3)
             ON CONFLICT(id) DO UPDATE SET updated=?3,
               title=CASE WHEN session.title='' THEN ?2 ELSE session.title END",
            params![id, title, now],
        )?;
        Ok(id.to_string())
    }

    pub fn session_touch(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET updated=?2 WHERE id=?1",
            params![id, crate::now()],
        )?;
        Ok(())
    }

    pub fn session_status(&self, id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET status=?2, updated=?3 WHERE id=?1",
            params![id, status, crate::now()],
        )?;
        Ok(())
    }

    pub fn sessions(&self, limit: i64) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(
            "SELECT s.id, s.title, s.created, s.updated, s.status,
                    (SELECT COUNT(*) FROM turn t WHERE t.session=s.id)
             FROM session s ORDER BY s.updated DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map(params![limit], |r| {
                Ok(Session {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    created: r.get(2)?,
                    updated: r.get(3)?,
                    status: r.get(4)?,
                    turns: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Append one turn. The transcript is the durable record of the session.
    pub fn turn_add(&self, t: &Turn) -> Result<()> {
        self.conn.execute(
            "INSERT INTO turn(session,seq,ts,role,harness,model,text,run_id,ok)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                t.session,
                t.seq,
                t.ts,
                t.role,
                t.harness,
                t.model,
                t.text,
                t.run_id,
                t.ok as i64
            ],
        )?;
        self.session_touch(&t.session)?;
        Ok(())
    }

    pub fn turns(&self, session: &str, limit: i64) -> Result<Vec<Turn>> {
        let mut st = self.conn.prepare(
            "SELECT session,seq,ts,role,harness,model,text,run_id,ok FROM turn
             WHERE session=?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let mut rows = st
            .query_map(params![session, limit], |r| {
                Ok(Turn {
                    session: r.get(0)?,
                    seq: r.get(1)?,
                    ts: r.get(2)?,
                    role: r.get(3)?,
                    harness: r.get(4)?,
                    model: r.get(5)?,
                    text: r.get(6)?,
                    run_id: r.get(7)?,
                    ok: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Next sequence number for a session. Safe because the write is immediate.
    pub fn next_seq(&self, session: &str) -> Result<i64> {
        let n: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(seq) FROM turn WHERE session=?1",
                params![session],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(n.unwrap_or(0) + 1)
    }

    /// Everything that must be replayed into a resumed run: the tail of the
    /// transcript plus the live memory scoped to the session.
    pub fn resume_context(&self, session: &str, turns: i64) -> Result<String> {
        let mut out = String::new();
        let hist = self.turns(session, turns)?;
        if !hist.is_empty() {
            out.push_str("## Conversation so far (oldest first)\n");
            for t in hist {
                let who = if t.harness.is_empty() {
                    t.role.clone()
                } else {
                    format!("{}:{}", t.role, t.harness)
                };
                out.push_str(&format!("\n### [{}] {}\n{}\n", t.seq, who, t.text));
            }
        }
        let keys = self.list(&format!("session:{session}"), "", 100)?;
        if !keys.is_empty() {
            out.push_str("\n## Session facts (last version wins)\n");
            for (k, v) in keys {
                out.push_str(&format!("- {k} = {v}\n"));
            }
        }
        Ok(out)
    }

    // ---- runs -----------------------------------------------------------

    pub fn run_start(&self, id: &str, session: &str, task: &str, plan: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO run(id,session,task,plan,status,started) VALUES(?1,?2,?3,?4,'running',?5)",
            params![id, session, task, plan, crate::now()],
        )?;
        Ok(())
    }

    pub fn run_finish(&self, id: &str, status: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE run SET status=?2, finished=?3, detail=?4 WHERE id=?1",
            params![id, status, crate::now(), detail],
        )?;
        Ok(())
    }

    pub fn runs(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let mut st = self.conn.prepare(
            "SELECT id,session,task,status,started,finished FROM run
             ORDER BY started DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map(params![limit], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "session": r.get::<_, String>(1)?,
                    "task": r.get::<_, String>(2)?,
                    "status": r.get::<_, String>(3)?,
                    "started": r.get::<_, String>(4)?,
                    "finished": r.get::<_, Option<String>>(5)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
